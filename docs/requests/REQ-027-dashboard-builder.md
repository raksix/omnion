# REQ-027 — Dashboard Builder

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` (Studio)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Power BI / Notion-style dashboards with drag-and-drop widgets:

```text
Dashboard
├── Chart
├── Table
├── KPI
├── Map
├── Calendar
├── Activity
└── AI Widget
```

Example layout:

```text
┌─────────────┬─────────────┐
│ Revenue     │ Customers   │
│ $482K       │ 12,421      │
├─────────────┴─────────────┤
│       Sales Chart         │
├───────────────────────────┤
│ AI Insights               │
└───────────────────────────┘
```

## Implementation spec

> Buildable contract. Storage + query: new crate `crates/dashboards` (`omnion-dashboards`). HTTP surface: `apps/api/src/routes/dashboards.rs`. Panels under `/dashboards` in `apps/admin` (`apps/admin/features/dashboards/`). The layout is a 12-column grid persisted in integer units, so one saved layout renders identically at any viewport width.

### Scope (in / out)

**In**

- Dashboard CRUD, duplication, and a list screen with owner/visibility columns.
- Drag-and-drop grid: move, resize, reorder, duplicate and delete a widget; layout saved as units.
- Widget kinds: `chart` (line, bar, area, stacked bar, pie, donut), `table`, `kpi`, `map`, `calendar`, `activity`, `ai`.
- Data sources: analytics metrics (REQ-007), records of a dynamic entity (REQ-026), content and media counts, workflow executions (REQ-016), webhook deliveries, AI Hub chat (REQ-001).
- Dashboard filters (date range plus up to three select filters), manual refresh, auto-refresh interval, per-widget "updated at" footer.
- Read-only published link (token) for embedding a dashboard in the public renderer (docs/03-FRONTEND.md).
- Permissions, organization scope and optional site scope.

**Out** — cross-dashboard drill-through, threshold alerts, scheduled e-mail delivery (REQ-028 owns scheduling), third-party widget plugins, formula fields beyond a dataset's own aggregates, anonymous public dashboards without a token.

### Screens (UI)

| Route | Screen | Contents |
|---|---|---|
| `/dashboards` | List | Table: Name, Owner, Widgets, Visibility, Updated. Filters: visibility, owner, text. Actions: New, Duplicate, Copy link, Rename, Delete. Empty state "Create your first dashboard". |
| `/dashboards/{id}` | View mode | Widget grid from the saved layout; filter bar (Today / 7 days / 30 days / Quarter / Custom, plus up to three select filters); Refresh; auto-refresh selector (Off / 30 s / 5 min); per-widget menu (Refresh, Download CSV for tabular widgets); "Edit" toggle. |
| `/dashboards/{id}?edit=1` | Edit mode | Drag handles, resize handles, "+ Add widget" tile, keyboard nudging, per-widget settings panel, Save / Discard, unsaved-changes guard on navigation. |
| `/dashboards/{id}/widgets/new` | Widget picker | Sheet listing the seven kinds with a one-line description and a preview glyph; picking one opens the settings panel with kind-specific fields. |
| `/dashboards/{id}/widgets/{widgetId}` | Widget settings | Title, Kind (fixed after create except within a compatible family), Data source (dataset → metric/field → aggregate), Filters, Visual options (palette, legend, axis labels, number format, decimals), Size in grid units, own refresh override. Live preview while editing. |
| `/dashboards/{id}/share` | Share dialog | Create/revoke a read-only link, copy, expiry date, explicit warning that the token exposes the dashboard's current data without sign-in. |
| `/public/dashboards/{token}` | Public view | Read-only, no panel chrome, mobile-first single column, refreshed on load only. |

- Empty: no dashboards → single CTA; dashboard without widgets → "Add your first widget" tile; widget without rows → in-panel "No data for this period" with a hint to widen the range.
- Loading: skeleton widget cards with stable heights so the grid never jumps; refresh shows a header spinner; Save shows a spinner and disables.
- Error: one failed widget keeps the rest usable and shows a short reason plus Retry; dashboard 403/404 states; a failed layout save keeps the local changes and reports the error.
- Keyboard: `⌘S` save, `Esc` cancel drag or close a panel, `Delete` remove the focused widget (confirm), `D` duplicate focused widget, arrows nudge one grid unit (`⇧`+arrows resize), `/` focus widget-picker search, `R` refresh all.
- Mobile: view mode reflows to one column in saved order; edit mode is disabled below `md` with the inline note "Editing is available on tablet and desktop — you can still view and filter here."; filters open in a sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/dashboards` | List dashboards in scope | `dashboards.read` |
| POST | `/dashboards` | Create dashboard (name, description, visibility, site) | `dashboards.manage` |
| GET | `/dashboards/{id}` | Dashboard with widgets and layout | `dashboards.read` |
| PUT | `/dashboards/{id}` | Update metadata and/or the widget list | `dashboards.manage` |
| DELETE | `/dashboards/{id}` | Delete dashboard and widgets | `dashboards.manage` |
| POST | `/dashboards/{id}/duplicate` | Copy to a new dashboard | `dashboards.manage` |
| PUT | `/dashboards/{id}/layout` | Persist layout only (drag/resize save) | `dashboards.manage` |
| PATCH | `/dashboards/{id}/widgets/{widget_id}` | Update one widget's config | `dashboards.manage` |
| POST | `/dashboards/{id}/widgets/{widget_id}/data` | Run one widget query with current filters | `dashboards.read` |
| GET | `/dashboards/{id}/data` | Batch data for every widget in one request | `dashboards.read` |
| POST | `/dashboards/{id}/share` | Create a read-only token | `dashboards.manage` |
| DELETE | `/dashboards/{id}/share/{token_id}` | Revoke a token | `dashboards.manage` |
| GET | `/public/dashboards/{token}` | Read-only payload for the public renderer | — (token, rate-limited) |

Widget data never bypasses the caller's own permissions: the batch endpoint resolves each widget against the session's effective permissions and returns a per-widget error object instead of data when the source is forbidden, so one inaccessible source cannot blank the whole dashboard.

### Data model

`database/migrations/0012_dashboard_builder.sql` (numeric prefix = next free slot at tick time). Additive-only.

- `dashboards` — `id uuid pk`, `organization_id uuid not null → organizations on delete cascade`, `site_id uuid → sites on delete set null`, `name text not null`, `slug text not null`, `description text`, `visibility text not null default 'organization'`, `auto_refresh_seconds integer not null default 0`, `grid_version integer not null default 1`, `created_by uuid → users on delete set null`, `created_at`, `updated_at`. Checks: slug format, `visibility in ('private','organization','link')`, `auto_refresh_seconds in (0, 30, 300)`; `unique (organization_id, slug)`.
- `dashboard_widgets` — `id uuid pk`, `dashboard_id uuid not null → dashboards on delete cascade`, `kind text not null`, `title text not null`, `position_x integer not null`, `position_y integer not null`, `width integer not null`, `height integer not null`, `sort_order integer not null default 0`, `config jsonb not null default '{}'::jsonb`, `data_source jsonb not null default '{}'::jsonb`, `created_at`, `updated_at`. Checks: `position_x >= 0`, `position_x + width <= 12`, `width` 1–12, `height` 1–12, `kind` within the seven kinds. Index `dashboard_widgets_dashboard_idx (dashboard_id, sort_order)`.
- `dashboard_shares` — `id uuid pk`, `dashboard_id uuid not null → dashboards on delete cascade`, `token_hash text not null unique` (only a hash is stored), `created_by uuid`, `expires_at timestamptz`, `revoked_at timestamptz`, `created_at`.

Grid units convert to pixels through the panel's fixed column width and gutter from the design tokens, so a saved layout is device-independent. The column count is versioned in `grid_version`.

### Events

- Emitted: `dashboard.created`, `dashboard.updated`, `dashboard.deleted`, `dashboard.widget.updated`, `dashboard.link.created`, `dashboard.link.revoked`.
- Consumed: none required — widget data is pulled on demand. Once REQ-041 (real-time platform) lands, `activity` and `kpi` widgets subscribe to a stream instead of polling.
- Webhook relevance: low to medium. Events describe layout and sharing changes; data rows are never part of a payload, so a subscriber cannot learn a dashboard's contents from the bus.

### Acceptance criteria

- [ ] A dashboard can be created, renamed, duplicated and deleted from `/dashboards`.
- [ ] The list shows live widget counts and the owner; filters narrow the list correctly.
- [ ] All seven widget kinds can be added and render real platform data (no hard-coded series).
- [ ] Adding a widget, dragging it, resizing it and saving survives a full page reload.
- [ ] The saved layout reflows without overlap at 1440, 1024 and 390 px.
- [ ] Widget settings changes (title, metric, aggregate, number format, palette) apply to the live preview and persist.
- [ ] Date-range and select filters change the data of every widget that declares them.
- [ ] Manual refresh and auto-refresh both update the "updated at" footer.
- [ ] A widget whose source is forbidden renders a per-widget permission message while the rest of the dashboard loads.
- [ ] A widget with no rows shows the "No data for this period" state instead of an empty box.
- [ ] A read-only link renders the dashboard without a session and stops working after revoke or expiry.
- [ ] Discarding unsaved edits leaves the persisted layout untouched; navigating away with unsaved changes warns.
- [ ] Keyboard nudging moves and resizes the focused widget one grid unit at a time.
- [ ] `cargo test -p omnion-dashboards` covers layout validation, visibility rules and token revocation.
- [ ] `pnpm typecheck && pnpm build` pass and the chart dependency stays inside the panel's bundle budget.
- [ ] Every `/dashboards` route appears in the QA walkthrough inventory and every visible control is clicked.
- [ ] Mobile 390 px view mode renders one column with working filters; edit mode shows the read-only note.
- [ ] A scope test with two organizations seeded proves no dashboard payload leaks another organization's rows.

### QA plan

Add `/dashboards` and its child routes to the QA walkthrough route list, then walk:

1. Empty state → create "Sales overview" → add a KPI widget (metric: revenue), a chart widget (line, 30 days), a table widget (recent records) and an activity widget.
2. Drag the chart below the KPI pair, resize the table to full width, save, reload and confirm the layout is identical.
3. Switch the date range to 7 days and confirm KPI numbers and chart axis labels change; toggle auto-refresh off.
4. Open widget settings, change the number format and palette, and watch the preview update.
5. Create a read-only link, open it in a fresh context without a session, then revoke it and confirm it is gone.
6. Signed in with a limited role: a dashboard containing one forbidden source shows a per-widget message while the other widgets render.

The visual check must see: KPI values on one type scale with aligned baselines, a chart with readable axis labels and a legend, no overlapping or clipped widget cards, a sensible first-load empty state, and no horizontal page scroll at 390 px.

### Slices

1. **Dashboards + grid view** — migration, dashboard CRUD API, list screen, read-only grid renderer with the KPI, chart and table widgets against real metrics. Done when a dashboard survives a reload with correct values and layout.
2. **Editing** — drag/resize/reorder, widget picker, widget settings panel, layout endpoint, unsaved-changes guard, keyboard nudging. Done when move + resize + save is verified by reload and by a layout-API round trip.
3. **Remaining widgets + sharing** — map, calendar, activity and AI widgets, dashboard filters and refresh controls, share tokens and the public route. Done when all seven kinds render real data and a revoked link stops working.

### Risks / notes

- Charting dependency: pick one library for the whole panel and stay inside the bundle budget; a different library per widget would double the bundle.
- Widget queries share datasets with REQ-028, so a runaway aggregate could slow the API. Every widget query gets a row cap and a short statement timeout, and the batch endpoint caches per (widget, filter) for the refresh interval.
- The AI widget consumes tokens; it must go through the AI Hub with cost accounting (REQ-047) and be labelled as AI-generated in the UI.
- Overlap policy: the client prevents dropping a widget onto an occupied area, and the server validates bounds, returning 422 with the offending widget id so a buggy client cannot corrupt a grid silently.
- Share tokens are read-only and must expose rendered values only — never SQL, dataset names or record identifiers of other entities.
- Edit mode below `md` is intentionally disabled rather than degraded: dragging on a phone produces corrupt layouts, and a half-working editor is worse than a clear note.
- Auto-refresh pauses when the tab is hidden, or the panel polls the API all night.
- Layout units, not pixels, are the contract; changing the token grid later would silently reflow every saved dashboard, which is why `grid_version` exists.
