# REQ-032 — Universal Command Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + core search
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

`Ctrl + K`:

```text
Search or command...

> Create customer
> Open CRM
> Create workflow
> Switch site
> Search invoice 4812
> Run backup
> Open analytics
> Ask AI
```

And natural language resolves to results directly:

> "Open Mehmet's last 10 tickets."

## Notes

- Extends the command palette from REQ-002; commands must respect the caller's permissions
  (docs/07-IAM.md) and audited like any other action.

## Implementation spec

### Scope (in / out)

In:

- One global palette overlay available on every admin route on `Ctrl+K` / `Cmd+K`, opening over the current page without losing its state.
- Three input behaviours from a single box: navigation ("Open analytics"), actions ("Create customer", "Run backup", "Switch site"), and entity search ("Search invoice 4812") with grouped, ranked results.
- Prefix narrowing: `>` commands only, `@` people, `#` sites, `:` settings, `?` shortcut help.
- Natural-language resolution ("Open Mehmet's last 10 tickets") through ai-hub into a structured intent (entity, filters, sort, count, target route or command), which is shown for confirmation and then executed by the same permission-checked path as a manual command.
- Server-side permission filtering of both commands and results — the palette never renders a command or a record the caller cannot open (built on the REQ-002 index plus the permissions crate).
- A command registry contributed by core and by feature modules (id, title, group, required permission, target route or handler, keywords, aliases, icon).
- Per-user recents plus route-context suggestions.
- Audit of every action executed from the palette.

Out:

- Multi-turn conversation and agentic task execution (AI Copilots, REQ-042; AI App Builder, REQ-045).
- Inline editing from a result row — results open the record; editing happens in the owning screen.
- New indexing infrastructure (owned by REQ-002); this REQ reads that index.
- Voice input, browser extensions, and an unauthenticated public search endpoint.
- Personalised ranking beyond recents and route context (no model-based ranking in v1).

### Screens (UI)

Overlay component `apps/admin/components/command-center.tsx`, mounted once in the app shell so `Ctrl+K` works on every route, including over open modals. Plus one shareable page:

```text
/search?q=&types=&site=&page=    ← full-page results (deep-link target)
```

- Palette layout: centred dialog, max width 640px, top offset 12vh; one input row, a grouped result list, and a footer hint row (`↑↓` navigate · `↵` open · `Tab` next group · `Esc` close). Groups: Commands, one group per result type, the AI resolution card, Recent.
- Result row anatomy: icon, title with match highlighting, subtitle (breadcrumb, type, metadata), right-aligned badge or shortcut. Keyboard selection and mouse hover share the same visible focus treatment.
- AI resolution card: renders the interpreted intent in plain words ("Tickets · assignee: Mehmet · last 10 · newest first") with `Run` and `Edit as search`, so the operator can see and correct the interpretation before anything executes.
- `/search` page: the same groups in a full page with a header search input, result count, active filter chips, and pagination. Filters: type checkboxes (Pages, Posts, Customers, Invoices, Tickets, Users, Media, Settings, Logs), site select, date range, owner/assignee — all in the URL.
- Bulk actions on `/search`: read-only surface, so `Export results` (CSV, thin reuse of REQ-031) and per-row `Copy link` only.
- Empty states: empty query shows Commands + Recent; no results shows "Sonuç yok" with a spelling suggestion, a "Search everything" action and "Ask AI"; results that exist but are all hidden by permissions show "N sonuç yetkiniz dışında" without any titles.
- Loading: result groups stream independently — commands render instantly from the local registry, entity groups show skeletons, the AI card shows a resolving state. Errors: one retryable inline error per group with the request id in a tooltip; a failing type must never blank the palette.
- Keyboard: `Ctrl+K` / `Cmd+K` toggle, `Esc` close with focus restored to the invoking element, `↑`/`↓` move, `Enter` open/run, `Cmd+Enter` open in a new tab, `Tab`/`Shift+Tab` cycle groups, prefix keys `>` `@` `#` `:` `?`, and `?` on an empty input opens the shortcut sheet.
- Mobile: opened from the top-bar search field (no hardware shortcut), presented as a full-screen sheet; results cap per group with a `show all` row, touch targets ≥44px, and the AI card sits above the results as a full-width card.

### API

| Method | Path | Purpose | Permission |
| --- | --- | --- | --- |
| GET | `/api/v1/search?q=&types=&site=&cursor=&limit=` | Federated, grouped, permission-filtered search | `search.read` |
| GET | `/api/v1/search/suggest?q=` | Instant suggestions (titles, commands, shortcuts) | `search.read` |
| GET | `/api/v1/commands` | Commands the caller may run, with keywords and aliases | `search.read` |
| POST | `/api/v1/commands/{id}/run` | Execute a command; the command re-checks its own permission | the command's permission, e.g. `crm.customers.create` |
| POST | `/api/v1/command-center/resolve` | Natural language → structured intent plus preview text | `search.read` |
| GET | `/api/v1/command-center/recent` | Caller's recent queries and commands | `search.read` |
| DELETE | `/api/v1/command-center/recent` | Clear recents | `search.read` |
| GET | `/api/v1/command-center/context?route=` | Suggested commands for the current route | `search.read` |

Notes: a navigation command returns the resolved route and writes nothing; a mutating command goes through the owning feature's service and returns that feature's response shape. `resolve` returns `{intent, confidence, preview_text, route?, command_id?, filters?}` and never executes anything.

### Data model

Migration `database/migrations/0012_command_center.sql`.

`command_recents`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial pk` | |
| `user_id` | `uuid not null` | fk `users` |
| `organization_id` | `uuid not null` | fk `organizations` |
| `kind` | `text not null` | check in (`query`,`command`) |
| `query` | `text` | typed text, trimmed to 200 chars |
| `command_id` | `text` | registry id when `kind = command` |
| `result_count` | `integer` | for queries |
| `created_at` | `timestamptz not null default now()` | |

Indexes: `(user_id, created_at desc)`, unique `(user_id, kind, query)` for upsert-on-repeat. Trim to the newest 50 rows per user.

`command_usage_daily`: `user_id uuid`, `organization_id uuid`, `command_id text`, `day date`, `runs integer not null default 0`, primary key `(user_id, command_id, day)` — powers suggestions and adoption reporting; stores no query text.

The command registry has no table: command definitions are compiled into the core crate and feature modules and exposed through a permission-filtered projection (documented decision — commands are code, not configuration, in v1). The entity index itself belongs to REQ-002.

### Events

Emitted: no bus events for navigation or plain searches (volume would drown real facts). Mutating commands emit the owning feature's domain event unchanged (for example `customer.created`).

Consumed: `page.published`, `customer.created`, `invoice.created`, `ticket.created` — used only to invalidate the palette's per-group caches so freshly created records are findable within seconds; consumption writes no data.

Webhook relevance: none directly. Audit: every action command writes a `command.run` audit entry with actor, command id, target type/id, result status and request id; `resolve` calls are logged with the interpreted intent (no row data) so AI interpretations can be reviewed.

### Acceptance criteria

- [ ] `Ctrl+K` (and `Cmd+K`) opens the palette on every admin route, including over an open modal, without losing unsaved form state.
- [ ] `Esc` closes the palette and returns focus to the element that had focus before opening.
- [ ] Typing a partial page title returns that page under the entity groups within 300 ms p95 on the seeded dataset.
- [ ] Command lists are filtered server-side: a role without `crm.customers.create` never receives "Create customer".
- [ ] Search results are permission-filtered per type; requesting a shared `/search` URL as a lower-privilege user returns no title text for records they cannot read.
- [ ] `>` shows commands only, `@` people only, `#` sites only, and the mode chip in the input row reflects the active prefix.
- [ ] Running "Open analytics" navigates without a full reload and closes the palette.
- [ ] "Create customer" opens the create form with the palette closed and the first field focused.
- [ ] "Run backup" asks for confirmation before queueing a job, then shows the standard job feedback.
- [ ] The AI card for "Open Mehmet's last 10 tickets" shows the parsed intent (entity, assignee, count, sort) before running, and `Run` lands on a ticket list matching that intent.
- [ ] A low-confidence phrase never auto-executes: it renders as "Did you mean …" with the closest commands.
- [ ] A failing result type shows a per-group error with retry while other groups still render.
- [ ] Recents persist across sessions and are per user, not per organization.
- [ ] Clearing recents empties the group immediately and after a reload.
- [ ] Every executed action command appears in the audit log with actor, command and target.
- [ ] `/search` reproduces the same results from its URL alone, with filter chips and type filters applied.
- [ ] Keyboard-only operation reaches every group and row; `Tab`/`Shift+Tab` cycle groups and `Cmd+Enter` opens in a new tab.
- [ ] Mobile: the top-bar search opens a full-screen sheet, all targets are ≥44px, and the AI card is usable at 390×844.

### QA plan

Browser walkthrough:

1. On `/` press `Ctrl+K` → palette opens; type "ana" → "Analytics" appears; `Enter` navigates and the palette closes.
2. Re-open and type "invoice 4812" → the seeded invoice appears with its number highlighted; `Cmd+Enter` opens it in a new tab.
3. Type `>` → only commands list; run a destructive command and confirm the confirmation step appears before queueing.
4. Type "Open Mehmet's last 10 tickets" → the AI card shows the parsed intent; `Edit as search` turns it into filter chips on `/search`; `Run` lands on a matching ticket list.
5. Type "zzqqxx" → empty state offers the spelling suggestion and "Ask AI"; no error toast.
6. Sign in as a limited role → "Create customer" is absent, and a shared `/search` link shows "N results outside your permissions" with no titles.
7. Simulate one failing type (block that request) → that group shows a retryable error, the others render.
8. Keyboard-only pass: open, move, `Tab` between groups, `Esc`, and confirm unsaved form input survives.
9. Mobile viewport 390×844: top-bar search opens the sheet; the AI card and result rows are comfortably tappable.
10. Check `/audit` after steps 3, 4 and 6 → exactly one entry per executed command with the right actor and target.

Visual check: the palette reads as a clearly layered dialog over a scrim; match highlighting is visible in both themes; the AI card looks like a proposal, distinct from results; group headers and type badges are consistent; empty states are not styled as errors (only real failures are).

### Slices

1. **Palette shell + registry + navigation.** Overlay, keyboard model, local command registry (navigation and simple actions), `/api/v1/commands` with permission projection, recents persistence, `/search` page skeleton.
   Done: `Ctrl+K` opens everywhere, navigation commands run, and a limited-role test proves commands are filtered.
2. **Federated search in the palette.** `/api/v1/search` plus suggest wiring, grouped results with per-group loading and errors, type filters, URL-backed `/search`.
   Done: browser steps 1, 2, 5, 6 and 7 pass and results respect permissions.
3. **Action commands + audit.** Mutating commands (create customer, create workflow, run backup, switch site) through the owning services, confirmation for destructive ones, `command.run` audit entries, `command_usage_daily`.
   Done: steps 3 and 10 pass and the audit log shows one entry per action.
4. **Natural-language resolution.** `POST /command-center/resolve` on ai-hub, intent preview card, confidence threshold with "Did you mean", `Edit as search`, timeout fallback.
   Done: step 4 passes, a low-confidence phrase falls back instead of executing, and a slow model shows a resolving state without blocking typing.

### Risks / notes

- Permission leakage is the top risk: filtering must happen in the query layer, not the component; keep a test asserting that filtered-out titles are absent from the response body, not merely hidden.
- AI latency varies: resolve fires after a 250 ms typing pause, is cancellable, and degrades to plain search when slow or unavailable.
- Index staleness after mutations: the consumed events shrink the window; the palette shows an "updated just now" context on entity groups instead of pretending to be real time.
- Keystroke capture must not break typing: the palette opens on `Ctrl+K` only (never a bare letter), and prefix characters are interpreted only inside the palette input.
- Recents store raw query text, which may contain customer names: trim, cap, keep per user, clearable via `DELETE`, and excluded from analytics exports.
- Command ids are API surface: renaming one is a breaking change for automation that references it, so ids stay stable and old titles become aliases.
- The palette must stay usable without heavy effects: results render as a plain list, animations are transform-only, and reduced-motion preferences are honoured.
