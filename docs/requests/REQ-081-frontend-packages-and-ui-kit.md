# REQ-081 — Frontend Packages & UI Kit

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `packages/*`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The shared frontend foundation every app consumes.

- `@omnion/ui`: button, input, modal, dropdown, tabs, card, table, dialog, toast, empty state, skeleton.
- Component override mechanism so a theme or module can restyle without forking.
- `@omnion/types` (contracts), `@omnion/api-client` (typed fetchers), `@omnion/theme-sdk`.
- Frontend module SDK: a module can ship admin screens, blocks and settings panels.
- Editor UI package: node canvas graph layout, CodeMirror 6 integration, expression editor.
- Frontend insights + OpenTelemetry module (client error + performance reporting).

## Implementation spec

### Scope (in / out)

**In**

- `packages/ui` (`@omnion/ui`) — the kit docs/03-FRONTEND.md names: Button, Input, Textarea, Select, Checkbox, Switch, Radio group, Modal, Dialog, Drawer, Dropdown/Menu, Tabs, Card, Table, Pagination, Toast, Tooltip, Badge, Avatar, Breadcrumbs, EmptyState, Skeleton. The panel's hand-rolled components (`apps/admin/components/{app-shell, empty-state, loading-table, status-badge, site-switcher, global-search, search-palette}`) are the first consumers: each becomes a thin wrapper over the kit and is deleted once its last screen moves.
- One public surface per component: typed props, `size` (`sm|md|lg`), `variant`, merged `className` (no CSS-in-JS), `asChild` for links, forwarded refs, controlled and uncontrolled forms, generated ids wired to `aria-describedby` for help and error text.
- Every component ships its states — default, hover, focus-visible, active, disabled, loading, invalid, read-only — plus the keyboard and ARIA behaviour of its WAI-ARIA pattern (menu, dialog, tabs, combobox, tooltip, sortable table). A component without states and a keyboard note does not land.
- Styling: Tailwind 4 (already in `apps/admin`) over the design tokens (`@omnion/tokens`, REQ-085) exposed as CSS custom properties; `apps/web` and `themes/*` consume the same tokens and override them per theme. No component carries a literal colour.
- Component override mechanism: `UiProvider` takes `components: Partial<UiComponents>` and `tokens`; resolution order is theme override → module override → kit default, resolved once per render tree, typed so an override cannot silently drop a required prop. Themes override tokens first, components second; a module may override only the components it owns.
- `packages/types` stays types-only: it grows the public shapes the packages need and, more importantly, a fixture set (`packages/types/fixtures/*.json`) of captured API responses that a Node test type-checks, paired with an API-side golden test over the same DTOs, so a contract change fails loudly instead of drifting.
- `packages/api-client` (`@omnion/api-client`) — typed fetchers for the panel: `api.get/post/patch/delete`, cookie session, request-id echo, RFC 7807 problem details mapped to a typed `ApiError` (code, message, field errors), abort, one retry on network failure (never on 4xx/5xx mutations), list helpers (`page`, `per_page`, `q`, `sort`, filters) and thin `useQuery`/`useMutation` wrappers with cache, revalidation on focus and optimistic updates — so `apps/admin/lib` and `apps/web/lib/api.ts` stop hand-rolling fetch.
- `packages/plugin-sdk` frontend half: a module manifest may declare admin nav entries, code-split routes, blocks (registered with REQ-063), settings panels and dashboard widgets. Contributions are validated against schemas at boot; an unknown block type or a route colliding with a core path is refused with a readable error instead of rendering a broken screen.
- `packages/editor-ui`: graph canvas (pan, zoom, node drag, port hit-testing, edge routing, minimap, snapping, marquee select, undo/redo integration) for the workflow editor (REQ-086) and node library (REQ-087); CodeMirror 6 fields (JSON, expression, code) with lint, bracket matching and read-only mode; an expression editor completing from node outputs and variables (REQ-092).
- `packages/insights`: client error capture (`window.onerror`, unhandled rejections, React error boundary), Web Vitals (LCP, CLS, INP), API timings via the api-client interceptor, breadcrumbs (route, last actions, release) and an OpenTelemetry exporter option (`OTEL_EXPORTER_OTLP_ENDPOINT`, W3C trace context propagated on outbound API calls). Sampling is configurable; a server-side scrubber strips field values, e-mail addresses and tokens from messages and stacks, and a test proves it.

**Out**

- Publishing any package to a public registry: `packages/*` are workspace-only in v1; distribution is REQ-048's problem.
- Server-only modules: everything stays React 19 Server-Component compatible but ships nothing that only runs on the server.
- A separate documentation site (Storybook and friends): the in-app `/design-system` route is the deliverable.
- Rewriting every panel screen at once — the migration is screen by screen, and old components stay importable until their last consumer moves.
- Component-level visual regression and pixel diffing (REQ-085's harness owns that), and the marketplace install path for modules (REQ-044).

### Screens (UI)

- **`/design-system`** (admin, internal) — the kit's gallery: sidebar groups Inputs, Feedback, Navigation, Data, Layout; one card per component with every state rendered, its keyboard/ARIA notes, the tokens it reads and its override key. A header switcher toggles colour mode, density and radius so the page doubles as the theming smoke test.
- **`/design-system/tokens`** — read-only tables for colour, type scale, spacing, radius, shadow and motion with the resolved values per mode, rendered from `@omnion/tokens`.
- **`/settings/developer`** — module contribution inspector: each installed module with its nav entries, routes, blocks, settings panels and widgets; invalid contributions listed with the reason and a `reload registry` action.
- **Layout** — the admin shell is the reference implementation: sidebar, header with site switcher, command palette, page header with breadcrumbs and actions. Toasts land bottom-right on desktop and bottom-center on mobile with `role="status"`, a dismiss control and a dismiss-all shortcut.
- **States** — the gallery shows default, hover, focus-visible, disabled, loading, invalid, read-only and empty per component; the page itself has skeleton, empty (no search match) and error states, and each card sits behind its own error boundary so one throwing component never blanks the page.
- **Keyboard** — `⌘K`/`Ctrl+K` command palette, `g d` design system, `/` focus the gallery search, `Tab` mirrors DOM order, focus is trapped only inside modal/drawer and always returns to the trigger, `Esc` closes overlays.
- **Mobile (<1024px)** — sidebar becomes a sheet, gallery a single column with a sticky group switcher, tables become cards, dialogs become full-height sheets; no horizontal scrolling on any screen this request touches.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/insights/overview` | Client errors, Web Vitals and API timings for the panel (24h / 7d) | `insights.read` |
| GET | `/api/v1/insights/errors` | Grouped client errors with fingerprints, counts, last seen and release | `insights.read` |
| GET | `/api/v1/insights/errors/{fingerprint}` | One error group: occurrences, routes, releases, scrubbed stack samples | `insights.read` |
| POST | `/api/v1/insights/client` | Batched telemetry intake (errors, vitals, API timings) — sampled, rate-limited, body ≤ 64 KiB | none (public, rate-limited) |
| GET | `/api/v1/modules` · `/api/v1/modules/{key}` | Installed modules and the admin contributions each declares | `plugins.read` |

`insights.read` is new (category `insights`) and is added to `crates/permissions/src/catalogue.rs`, which stays the single source of truth; every route is guarded with `guards::require(...)`. Intake rejects unknown keys, stores only the scrubbed payload, and never accepts request bodies, field values or identity. The design-system and gallery screens are client-side and need no endpoint of their own.

### Data model

No package needs a table; telemetry does, and it must not become a raw log sink.

- Migration `0017_frontend_packages.sql` (next free number at merge; REQ-083 and REQ-084 also add migrations, numbering follows merge order):
  - `client_telemetry_settings` — `app text primary key check (app in ('admin','web'))`, `enabled boolean not null default true`, `sample_rate numeric(4,3) not null default 0.100 check (sample_rate between 0 and 1)`, `retention_days integer not null default 30 check (retention_days between 1 and 365)`, `updated_at timestamptz not null default now()`.
  - `client_telemetry_events` — `id bigserial`, `app`, `kind text not null check (kind in ('error','vital','api'))`, `occurred_at timestamptz not null`, `route text not null`, `name text not null` (metric or error class), `value double precision`, `release text`, `session_key text` (rotating, carries no identity), `fingerprint text`, `detail jsonb not null default '{}'` (post-scrub), with indexes on `(occurred_at desc)`, `(app, kind, occurred_at desc)` and `(fingerprint) where fingerprint is not null`.
  - `client_telemetry_buckets` — `bucket timestamptz not null`, `app`, `kind`, `route`, `count integer not null`, `p75 double precision`, `p95 double precision`, primary key `(bucket, app, kind, route)`; the overview reads buckets, never raw rows.
- Retention: a maintenance routine rolls raw events past `retention_days` into buckets and truncates them, resuming from a cursor so a restart does not reprocess.
- Contract fixtures: `packages/types/fixtures/*.json` are captured responses; a Node test type-checks them, an API golden test serialises the same DTOs and compares — a stale fixture is a bug, not a warning.

### Events

- None on the bus. The packages are build artefacts, and telemetry is deliberately not an event: its volume belongs in its own tables, not in the webhook delivery path, and a subscriber must not be able to receive visitor telemetry.
- The panel does not subscribe to anything for these packages; the contribution registry refreshes on navigation and on the explicit reload action, so installing a module needs no live socket.
- If telemetry thresholds must later drive automation, that is an aggregate-change concern for REQ-007 (analytics), not a new event here.

### Acceptance criteria

- [ ] `pnpm --filter @omnion/ui typecheck` and the kit's component tests pass; every component in the request's list exists with its states and a keyboard/ARIA note.
- [ ] `apps/admin` and `apps/web` both import `@omnion/ui`, `@omnion/types`, `@omnion/api-client` and `@omnion/tokens` from the workspace, and `pnpm build` succeeds for both.
- [ ] The panel's toast, dialog, table, empty-state and skeleton usages resolve to the kit; the duplicated components are gone and nothing imports them.
- [ ] `UiProvider` overrides work end-to-end: a test theme swaps Button and Card, and the gallery, the panel and the public renderer all show the override while untouched components keep the kit default.
- [ ] An override missing a required prop, or an unknown override key, fails `pnpm typecheck` instead of failing at runtime.
- [ ] `ApiError` parses code, message and field errors from a problem-details response; a 422 with field errors renders next to the matching inputs.
- [ ] List helpers build stable query strings, and the pages, media and audit tables use them (no manual filter concatenation left in those screens).
- [ ] A module declaring a block type absent from the REQ-063 registry is refused at boot with a named error, and the rest of the panel still loads.
- [ ] A module route colliding with a core path is refused, and the collision is reported on `/settings/developer`.
- [ ] The editor canvas supports pan, zoom, node drag, edge creation and undo/redo, with tests on the pure geometry helpers (no DOM required).
- [ ] The expression field offers completions from node outputs and variables; an invalid expression shows an inline error and blocks save.
- [ ] Client errors, Web Vitals and API timings reach `POST /api/v1/insights/client`; the scrubber test proves e-mail addresses, field values and tokens never reach storage.
- [ ] Sampling honours `sample_rate` (a burst at 0.1 stores roughly a tenth) and the ingested count is visible on `/api/v1/insights/overview`.
- [ ] The insights routes are guarded: a role without `insights.read` gets 403, a signed-out caller gets 401.
- [ ] `/design-system`, `/design-system/tokens` and `/settings/developer` follow the kit's own rules — keyboard reachable, focus-visible, correct landmarks, no layout shift on load.
- [ ] Telemetry tables stay inside their retention window after the sweep, and buckets still answer the 24h and 7d questions.
- [ ] Mobile pass at 390px on the gallery, the developer settings screen and every migrated panel screen: no horizontal scroll, controls reachable.
- [ ] `scripts/qa/run.sh` covers the new routes, reports zero high findings, and the new screens appear in the walkthrough inventory.

### QA plan

- **Walkthrough must click:** `/design-system` group switcher and search; per group one component with its disabled and loading states; the mode/density/radius switcher; a modal open → `Esc` → focus returned to the trigger; a toast raised twice then dismissed all; `/design-system/tokens` mode toggle; `/settings/developer` registry reload; and one migrated panel screen (pages or media) to prove the swap kept behaviour.
- **Visual check should see:** consistent spacing and type across kit components, a visible focus ring on every interactive element, no colour disagreeing with the tokens, toasts and dialogs aligned to the edges, skeletons matching the final layout blocks, and the same components standing up in both colour modes without contrast or overflow regressions.

### Slices

1. **Kit foundation, tokens, telemetry migration** — `packages/ui` with Button, Input, Select, Checkbox, Switch, Card, Badge, EmptyState, Skeleton, Toast; `UiProvider` with overrides; tokens wired into both apps; `0017_frontend_packages.sql` with the telemetry tables, settings and sweep.
   *Done when:* both apps build against the kit, the override test theme renders, and the sweep keeps telemetry inside its window.
2. **Data surfaces and panel migration** — Table, Pagination, Dialog, Drawer, Dropdown, Menu, Tabs, Tooltip, Breadcrumbs, Avatar; `@omnion/api-client` with `ApiError`, list helpers and hooks; pages, media, sites and search screens move off hand-rolled fetch and duplicated components.
   *Done when:* a 422 with field errors renders beside the right inputs, and no migrated screen touches raw `fetch`.
3. **Modules, editor, insights** — `packages/plugin-sdk` contribution validation, `/settings/developer`, `packages/editor-ui` (canvas primitives, CodeMirror fields, expression editor), `packages/insights` client with scrubber and sampling, plus the intake and overview endpoints.
   *Done when:* a sample module ships one admin screen, one block and one settings panel through the registry, and a forced client error appears, scrubbed, on the overview.
4. **Design-system gallery and hardening** — `/design-system` with the states matrix and per-card error boundaries, `/design-system/tokens`, keyboard and mobile passes, walkthrough inventory update.
   *Done when:* every component renders in every state in both modes and the walkthrough is green with the new routes.

### Risks / notes

- The kit's first consumer is the panel: a component that only works on `/design-system` is not done. Every component ships with at least one real screen using it.
- Tailwind in the admin app and whatever the renderer uses must agree on the token layer; if they diverge the product grows two visual languages. Tokens are generated once (REQ-085) and both consume them.
- Telemetry is the easiest place to leak data: closed intake schema, server-side scrubbing as well as client-side, and retention enforced by the sweep rather than by hope.
- Overrides must not become an accessibility bypass: the shared component tests run against overridden implementations too, so an override that drops the focus ring or the accessible name fails.
- `@omnion/api-client` is generated-client-shaped but hand-written in v1; when an OpenAPI document lands it is regenerated and this layer thins out — the panel must not need a rewrite for that.
- The types contract only works if fixtures are refreshed deliberately: a fixture update belongs to the same change as the DTO change.
- Keep the kit free of product concepts (no site, theme or tenant props): a component that needs `site_id` belongs in the app, not the kit.
