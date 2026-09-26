# REQ-002 — Global Search Engine

> **Status:** in-progress (slice 1 shipped; slice 2 next) · **Captured:** 2026-09-25 · **Layer:** core (`crates/search`) + admin UI
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
- [ ] `⌘K`/`Ctrl+K` opens the palette, `Esc` closes it, `↑`/`↓` traverse sections, `Enter` opens the highlighted row, `⌘Enter` opens a new tab.
- [ ] Fewer than two characters keeps the recent-search list; a query with no hits renders the no-results state.
- [x] Recent searches persist per account (≤20, pruned, clearable through `DELETE /search/recent`). *(Per-item removal is the palette's job — slice 2, client side; the store keeps the newest twenty per account.)*
- [ ] `/search` renders facets with counts, applies them as removable chips and paginates 50 per page with a correct total.
- [ ] Bulk selection supports Shift-range and `⌘A`; Copy links yields a newline-separated list; Export CSV produces one row per hit with the same count as the table.
- [x] Index maintenance works from the bus: publishing a page makes it findable within one runner tick, and deleting media removes its row. *(`media.deleted` has no producer yet, so the removal path is exercised directly — `remove_entity`, then a search that must not find it.)*
- [x] `POST /search/reindex` rebuilds a provider idempotently (two runs, same count) and is refused without `search.manage`.
- [ ] Ranking weights from the settings form change the order of a fixture result set (title-heavy query ranks the title match first).
- [ ] Every screen has empty, loading and error states; no dead control and no placeholder copy.
- [ ] The mobile pass renders the palette as a full-screen sheet with 44px rows and reachable bulk actions.
- [x] `cargo test --workspace` (421 passed), `pnpm typecheck && pnpm build` (2/2) and the QA walkthrough (49 clicks · 49 screenshots · 0 high findings · 0 vision issues, `qa-artifacts/20260926-134405`) pass.

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
