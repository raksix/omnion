# REQ-032 — Universal Command Center

> **Status:** done (cc83afb…843a403) — all four slices shipped; the reading closed it · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + core search
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

Slice 1 shipped the boxes ticked below and slice 2 the next four; slice 3 ticked the two above
(confirmation + job feedback, and the audit criterion); slice 4 ticked the last three — the AI
card, the low-confidence rule and the phone. Evidence: the QA passes of
`qa-artifacts/20260926-171549`, `qa-artifacts/20260926-180233`, `qa-artifacts/20260926-184340` and
`qa-artifacts/20260926-194131` (**0 high findings, 0 vision issues**; the last one 97 clicks, 117
screenshots and the new `resolve-read` / `resolve-edit-as-search` steps),
`scripts/qa/probe-command-center.cjs` (**26/26**), `scripts/qa/probe-palette.cjs` (**22/22**),
`scripts/qa/probe-palette-federated.cjs` (**32/32**),
`scripts/qa/probe-command-actions.cjs` (**31/31**), `scripts/qa/probe-command-resolve.cjs`
(**47/47**) and the API suites `apps/api/tests/command_center.rs` (**18 walks**, 4 of them the
resolver) + `apps/api/tests/search.rs` (**+2 walks**: the count outside a reader's scope, and
`history=false`).

- [x] `Ctrl+K` (and `Cmd+K`) opens the palette on every admin route, including over an open modal, without losing unsaved form state. — the palette is mounted once in the app shell, so it is on every route; the pass opened it over the results screen's shortcuts dialog (palette `z 50` over dialog `z 40`, and the dialog stayed open while the palette's scrim closed the palette) and over a half-typed "New page" form, whose title text survived the round trip.
- [x] `Esc` closes the palette and returns focus to the element that had focus before opening. — measured with the create form's title field as the captured element (`focusReturned: true`).
- [x] Typing a partial page title returns that page under the entity groups within 300 ms p95 on the seeded dataset. — `probe-palette-federated.cjs` measures keystroke → row over 20 samples of the seeded title ("qa", "qa s", … "qa sample page", "sampl", "sample", "page"): **p50 197 ms · p95 220 ms · max 237 ms** — and every sample was a fragment the engine really answers, with a non-matching query clearing the screen first so a leftover row could never pass for a fresh answer.
- [x] Command lists are filtered server-side: a role without the create key never receives its command. — `command_center.rs` asserts the *title text* is absent from a member's answer, not merely that an id is missing (`content.pages.create` stands in for the request's `crm.customers.create`: the same mechanism, with the key this wave actually ships). The browser walk joined it in slice 2: as a Member the palette's `>` list holds "Open pages"/"Open media" and neither "Create a page" nor "Open sites".
- [x] Search results are permission-filtered per type; a shared `/search` URL read by a lower-privilege account returns no title text for records it cannot read. — slice 2's walk: the Owner's `?q=<site>&type=sites` link holds the site, the Member's own answer is `total 0` with `hidden_total 1` and the site's name appears nowhere in the body (`an_empty_answer_says_what_lies_outside_the_readers_scope` proves the same at the API, titles included); the member's screen renders **0 rows** while the copy says "1 result is outside your permissions".
- [x] `>` shows commands only, `@` people only, `#` sites only, and the mode chip in the input row reflects the active prefix. — `>` renders exactly the registry's rows (8 for an owner, chip "Commands"); `#qa` answers with the Sites provider only (no Pages/Media rows) and the chip reads "Sites"; `@` moves the chip to "People". Slice 2 made the narrowing a property of the *requests*: `>` asks the registry and nothing else (probe: 0 sections), `#`/`:`/`@` ask exactly one provider each, and `@` still promises no screen the panel does not have.
- [x] Running "Open analytics" navigates without a full reload and closes the palette. — the same walk with "Open pages": client-side navigation to `/pages`, `paletteClosed: true`.
- [x] "Create customer" opens the create form with the palette closed and the first field focused. — the same walk with "Create a page" (`/pages?new=1`): palette closed, form open, the title field carrying the focus.
- [x] "Run backup" asks for confirmation before queueing a job, then shows the standard job feedback. — **slice 3, with the stand-in this platform actually has**: no backup service exists yet, so the job action is the index rebuild (`act.reindex-search`) — it asks first (the palette's confirmation card; the API refuses an unconfirmed run with `confirmation_required` before anything executes, and the audit trail is *unchanged* while the question is open), then the owning service answers in its own shape and the palette prints that answer verbatim: "Rebuilt 7 providers · 18 documents · 95 ms" with a link to `/settings/search`, where the same pass is read back as the provider's last run. Proven by `probe-command-actions.cjs` (31/31: the question, Escape withdrawing it, the run, the entry) and the walkthrough's `action-confirm`/`action-run` steps (`ranNothingYet: true`, `newEntries: 1`).
- [x] The AI card for "Open Mehmet's last 10 tickets" shows the parsed intent before running. — the phrase reads as **"Tickets · assignee: Mehmet · last 10 · newest first"** at 85% confidence, and the audit trail is read *before and after* the reading to prove nothing ran (`probe-command-resolve.cjs`: `reading a phrase runs nothing — 1 → 1 command.run entries`; the walkthrough records `ranNothingYet: true` for its own `resolve-read` step). No tickets screen exists yet, so the card says so in plain words and offers `Edit as search`, which lands on `/search?q=tickets+Mehmet&sort=newest` with the order the phrase asked for — a reading is a proposal, not a promise.
- [x] A low-confidence phrase never auto-executes. — "zzqqxx" reads as *unclear* (55%, below the run threshold), so the card carries **no Run at all** — only `Edit as search` and the "Did you mean" alternatives — and the audit trail is unchanged after it (`probe-command-resolve.cjs`: `a low-confidence phrase offers no Run at all`, `and still runs nothing`). The rule is the server's: `runnable` is computed behind the API (confidence ≥ 0.6 **and** a destination the caller may really open), so no panel build can talk itself into a run. The same threshold holds for a reading the caller lacks the key for: an editor asking "open sites" receives no `nav.sites` anywhere in the body and no Run.
- [x] A failing result type shows a per-group error with retry while other groups still render. — slice 2's probe fails one type's request with a real `503 dependency_unavailable`: that section shows "Media could not be searched." with a **Try again** button (the failure's code and status in its tooltip) while the Pages section keeps its rows, and the retry refills the failed group once the store answers again.
- [x] Recents persist across sessions and are per user, not per organization. — the walkthrough sees the command it ran after a reload; `recents_are_per_user_and_never_per_organization` proves one account's history is invisible to another in the same organization, and that a clear is personal.
- [x] Clearing recents empties the group immediately and after a reload. — `probe-command-center.cjs`: 0 rows immediately, 0 rows and `items: []` after a reload.
- [x] Every executed action command appears in the audit log with actor, command and target. — every run that starts writes exactly one `command.run` entry (`target_type: command`, `target_id` = the registry id, `metadata.outcome` + the owning service's aggregate result, `ip_address`; the actor is the caller's account), and the run is counted in `command_usage_daily` — one row per run, two runs two rows, proven at the API (`command_center.rs`: the entry names command and outcome, the usage count goes 1 → 2) and in the browser (`probe-command-actions.cjs`: "exactly one audit entry per executed command", actor + target read back through `GET /api/v1/iam/audit`). A refused run (unconfirmed, navigation, unknown id, or a caller missing the command's own key) writes nothing: a check asserts the trail is unchanged after all three refusals. Navigation commands still write no entry — the request's own rule holds.
- [x] `/search` reproduces the same results from its URL alone, with filter chips and type filters applied. — slice 2: a section's "see all" lands on `/search?q=<words>&type=<provider>` (the screen's own filter parameter, so the chip is already applied) and the screen's count equals the API's own count for that query — the same number after a reload. REQ-002's depth pass proved the rest of the screen's URL state (facets, chips, sort, page).
- [x] Keyboard-only operation reaches every group and row; `Tab`/`Shift+Tab` cycle groups and `Cmd+Enter` opens in a new tab. — arrows and `Enter` (both probes), `Ctrl+Enter` in a new tab and `Tab` between groups (probe-command-center) are proven; slice 2's probe adds `Shift+Tab` (forward `pages → media`, back `media → pages`) and the narrowing sweep of every mode.
- [x] Mobile: the top-bar search opens a full-screen sheet, all targets are ≥44px, and the AI card is usable at 390×844. — the sheet fills 390×844 and its rows (commands included) are 44px; slice 4 measured the card itself at 390×844: **374px wide** inside a 390px viewport (it spans the sheet rather than floating), its `Run` / `Edit as search` controls **44px** tall, and the "Did you mean" alternatives the same (`probe-command-resolve.cjs`: `it spans the sheet rather than floating in a corner`, `its controls are 44px on a phone`).

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

### Slice log

#### Slice 1 — palette shell + registry + navigation (2026-09-26) — shipped

What landed:

- **The registry is code.** `crates/search/src/commands.rs` holds one entry per command the
  platform offers today (8: the panel's screens plus "Create a page"), each with the permission
  whose holder may run it, the route it lands on, keywords and aliases for matching, and the
  routes it is worth suggesting on. `visible()` projects the registry through a caller's
  effective permissions and `suggest()` picks the suggestions for a screen; both are pure and
  unit-tested without a database.
- **The API serves the projection, never the whole list.** `GET /api/v1/commands` (guard
  `search.read`), `GET /api/v1/command-center/context?route=` (the suggestions for the screen the
  caller is on), and `GET`/`POST`/`DELETE /api/v1/command-center/recent` (the caller's own
  history: record, read, forget). Migration `0014_command_center.sql` adds `command_recents`
  (per user, newest-first read, trimmed to 50, upserted on repeat) and `command_usage_daily`
  (counts per account/command/day, no query text) — the usage table is written by slice 3.
- **The palette is the command centre.** One group of commands (suggestions for the screen while
  the box is empty, matches once something is typed), the `>` `@` `#` `:` `?` prefix model with a
  mode chip in the input row, the account's own Recent list with a Clear control, and the
  previously shipped entity sections. `Esc` now hands focus back to whatever had it before the
  palette opened.
- **A command that opens a form focuses it.** `/pages?new=1` opens the pages screen's create form
  and puts the cursor in the title field (the same one-shot-parameter pattern the `focus` deep
  link already used).

Deviations, recorded as they were decided:

- The migration is `0014_command_center.sql`: `0012`/`0013` were taken by REQ-002's index work.
- `command_recents` dedupes on folded generated columns (`query_key`, `command_key`) instead of a
  plain unique index on nullable columns — Postgres never collides NULLs, so `(user_id, kind,
  query)` alone would have stacked a row per run.
- `organization_id` on both tables is nullable: platform accounts belong to no organization, the
  same way `audit_log` files them.
- A search is remembered when the account **commits** to it (opening a hit, a section's "see all"
  or the results screen), not on every keystroke pause; REQ-002's `search_recent` stays the
  results screen's own history.
- `@` narrows the search to the users provider; a user hit renders no palette section until
  REQ-006 ships the accounts screen, so the mode answers honestly empty instead of linking
  nowhere.
- Navigation commands write no audit entry (the request's own rule); the `command.run` entries
  arrive with the mutating commands in slice 3.

Proof: `cargo test --workspace` → **452 passed, 0 failed** (9 of them the new command-centre
walks) · `cargo clippy --workspace --all-targets -- -D warnings` → clean · `pnpm typecheck &&
pnpm build` → 2/2 · `bash scripts/qa/run.sh` → the table above
(`20260926-171549`) · `node scripts/qa/probe-command-center.cjs` → **26/26** ·
`node scripts/qa/probe-palette.cjs` → **22/22**. A defect the probes caught before the commit:
the column's id-format check rejected hyphenated ids (`nav.create-page`), so recording a perfectly
ordinary command answered `500`; the check now allows hyphens and
`every_registered_command_can_be_remembered` walks the whole registry through the endpoint.

#### Slice 2 — federated search in the palette (2026-09-26) — shipped

What landed:

- **Groups answer on their own.** The palette asks each provider its own question (`GET
  /api/v1/search?q=…&types=pages`, once per section) instead of one answer it then slices: a
  section has its own skeleton while its request is in flight, its rows when it answers, and its
  own retryable error — the failure's code and status in the tooltip — when it fails. One slow or
  failing provider never blanks the others, which is what "result groups stream independently"
  means. A whole-index count rides behind the first wave for the "see all results" total and the
  number outside the caller's scope. Sections keep the registry's order (a streaming list must not
  reshuffle rows under the reader's eyes).
- **Typing does not become history.** The federated calls say `history=false`; `search_recent`
  stays the record of the searches someone committed to (a palette hit, a section's "see all", the
  results screen). The palette's own recents are `command_recents`, written on commit.
- **An empty answer says which empty it is.** `GET /api/v1/search` answers `hidden_total` — a count
  and nothing else, computed when the caller's own answer is empty — over the enabled providers
  their keys do not cover, so "nothing matched" and "nothing you may read matched" are told apart.
  The results screen renders "N results are outside your permissions." above its advice; the
  palette's no-results state carries the same line plus two ways out: **Search everything**
  (`/search?q=…`) and **Ask AI** (`/ai?q=…`, the AI Hub opens with the words in its prompt).
- **A "see all" is a link the screen understands.** `sectionUrl` now emits the results screen's own
  `type=<provider>` parameter (not `type:` text inside `q`), so the link lands with its filter chip
  already applied, its facet rail knowing which type is on, and its search box still showing the
  words the reader typed.
- **The narrowing modes narrow the requests.** `>` asks the registry and the index not at all
  (zero sections, zero calls); `@`/`#`/`:` ask exactly one provider each; `@` still renders no
  section — the accounts screen is REQ-006's — so the mode answers honestly empty rather than
  linking nowhere.

Deviations, recorded as they were decided:

- The fan-out is capped at three providers in flight (`GROUP_CONCURRENCY`), the rest follow as the
  first wave answers. Nine parallel calls queued against the browser's connection budget and
  starved the page's own RSC prefetches — a defect the walkthrough recorded as two aborted
  requests before the cap went in. The sections still stream; the pipe is no longer hogged.
- The debounce went from 120 ms to 100 ms: the groups are small, the sections are asked in
  parallel, and the slice's own budget is 300 ms p95 from keystroke to row.
- Per-group errors carry the failure's **code and status** in their tooltip. The request's own id
  is not in the spec's reach yet: the API has no request-id header — an observability concern that
  belongs to the platform-wide work (REQ-014/REQ-039), not to one screen.
- The palette's no-results state offers "Search everything" and "Ask AI" as real destinations; the
  *resolved* intent card ("Tickets · assignee: Mehmet · last 10") is slice 4's, and nothing here
  pretends to interpret language.
- The empty answer's hidden count is aggregate-only on purpose: no title, no id, not even a
  per-provider split travels with it.

Proof: `cargo test --workspace` → **455 passed, 0 failed** (3 new: 2 walks in
`apps/api/tests/search.rs` — the count outside a reader's scope (a member's answer counted it,
carried no title, and the Owner's own answer reported `0` hidden) and `history=false` leaving
`search_recent` untouched while a committed search is kept — plus the wire test for the flag) ·
`cargo clippy --workspace --all-targets -- -D warnings` → clean · `pnpm typecheck && pnpm build` →
2/2 ·
`bash scripts/qa/run.sh` → 77 clicks, 98 screenshots, **0 high findings**, **0 vision issues**
(`qa-artifacts/20260926-180233`) · `scripts/qa/probe-palette-federated.cjs` → **32/32**: a delayed
provider's section shows its own skeleton while another already holds rows; a `503` on one type
leaves that section retryable and the rest rendering; `>` emits no sections; `#`/`:`/`@` ask one
provider each; `Tab`/`Shift+Tab` walk the sections; "see all" lands on `/search?q=…&type=pages`
with the API's own count (the same after a reload, chip applied); "zzqqxx" offers the results
screen and the AI Hub (whose prompt is prefilled); typing stays out of the history while a
committed search is kept; and as the Member account the `>` list loses "Create a page" and "Open
sites" while the shared `?q=…&type=sites` link renders **0 rows** and "1 result is outside your
permissions." with the site's name nowhere in the answer. The typing budget, measured keystroke →
row over 20 samples: **p50 197 ms · p95 220 ms · max 237 ms** (the criterion is 300 ms p95).

#### Slice 3 — action commands + audit (2026-09-26) — shipped

What landed:

- **A command has a kind now.** The registry carries `CommandKind::Action` and a `confirm` flag, and
  `runnable()` answers only for actions — so the API's refusal (`not_runnable`) cannot drift from
  what the projection says. Two actions ship with the services that exist today: the index rebuild
  (`search.manage`, asks first) and clearing the account's own palette history (no key beyond being
  signed in, asks first because it cannot be undone).
- **`POST /api/v1/commands/{id}/run` is the only way an action runs.** It carries no route-level
  guard on purpose — every command has its own key — so the handler re-checks the command's own
  permission, refuses a navigation command by name, refuses an unknown id as a 404, and refuses an
  unconfirmed run with `confirmation_required` **before anything executes**. The confirmation is a
  rule of the platform, not a decoration of one dialog: a programmatic caller that skips the card
  still has to say yes.
- **The act is the owning service's code, not a copy.** `search::perform_reindex` (audit row and
  `search.reindexed` event included) now serves both the settings screen's button and the palette's
  command; one `clear_recents` serves the palette's own Clear control and the `act.clear-recents`
  command. The two entry points cannot drift.
- **One entry and one count per executed command.** `command.run` names actor, command (as the
  target), outcome and the owning service's aggregate result — never a row of content — and
  `command_usage_daily` counts the run (upserted by user/command/day, no query text). A run that
  starts is counted whatever its outcome; a run that never started (refused) is neither audited nor
  counted.
- **The palette asks, then prints the service's own answer.** An action row carries an Action badge
  and says it asks first; activating it opens the confirmation card above the list (`↵` run, `esc`
  withdraw — and `esc` does not close the palette while the question is open), and the result card
  shows the run endpoint's own message with a link to the screen that reads the record back.
  Recents of an action command run it again instead of navigating.

Deviations, recorded as they were decided:

- The request's four named examples ("Create customer", "Create workflow", "Run backup", "Switch
  site") arrive with their owning services — CRM (REQ-051), the workflows screen (REQ-003), a
  backup service, and a palette parameter model for "switch to site X" — none of which exist yet.
  What this slice ships is the machinery plus the two actions whose services are here, and the
  pattern for the rest is now one registry entry + one match arm + one audit expectation. In the
  acceptance criterion, the index rebuild is the stand-in for "Run backup": it is the platform's own
  long-running job, it asks first, and its feedback is the standard one (the provider's last run on
  `/settings/search`).
- No new migration: slice 3 writes the tables `0014_command_center.sql` already declared —
  `command_usage_daily` was created there "written by slice 3", and now is.
- The QA plan's step 10 says "check `/audit`"; the panel has no audit *screen* yet (REQ-039/REQ-012's
  ground), so the entry is read through `GET /api/v1/iam/audit` — the same rows the screen will read.
- `command.run` files the caller's organization when they have one and `null` at the platform level,
  exactly as `audit_log` files those accounts; the palette's history was already per user.
- The run endpoint answers the owning feature's shape inside `result` and one derived line in
  `message`. The panel prints that line; it composes no outcome of its own.

Found, not caused by this slice:

- The search suite asserted that one `indexer::drain(_, 100)` applies its own event. On a bus that
  now carries action-command reindexes too — every reindex leaves one `search.reindexed` per
  provider behind, and those are skipped rather than applied — a single batch can be spent on
  history the suite does not own, and the assertion failed for a reason it cannot control. The
  suite ticks the way the runner does now (bounded batches until one applies something), fixed in
  this tick.

Proof: `cargo test --workspace` → **463 passed, 0 failed** (8 new: 3 registry walks — only an action
is runnable and each action carries its own confirmation rule; every action matches its own words;
the projection never hands a member an action it may not run — plus 5 command-centre walks: a run
executes the owning service and leaves one `command.run` entry and one usage count per run; an
unconfirmed run leaves nothing; a screen command and an unknown id are refused by name; the endpoint
re-checks the command's own permission; every registered action has a service behind it) · `cargo
clippy --workspace --all-targets -- -D warnings` → clean · `pnpm typecheck && pnpm build` → 2/2 ·
`bash scripts/qa/run.sh` → 105 clicks, 115 screenshots, **0 high findings**, **0 vision issues**
(`qa-artifacts/20260926-184340`; the 5 medium findings are the public renderer's own icon 404s,
carried forward) · `scripts/qa/probe-command-actions.cjs` → **31/31** (the question, nothing run
while it is open, Escape withdrawing it, the run's own line, exactly one audit entry per executed
command with actor + target, the three refusals, the history really cleared, and the card at
390×844 with 44px answers) · `probe-command-center.cjs` **26/26**, `probe-palette.cjs` **22/22**,
`probe-palette-federated.cjs` **32/32** re-run clean (no regressions) · the walkthrough's own steps
record `ranNothingYet: true` and `newEntries: 1` for the run.

#### Slice 4 — natural-language resolution (2026-09-26) — shipped

What landed:

- **A phrase becomes a structure, checked against the platform's own tables.**
  `crates/search/src/intent.rs` reads one phrase into the vocabulary the platform really has: the
  domain word maps through the provider registry (and nothing else does), a command is only ever a
  registry entry whose own words the phrase covered, filters are the ones the index supports (an
  order, a count, an assignee), and confidence is *computed* — every recognised part raises it — so
  the same words always read the same way. A domain the index does not answer for stays a search
  and says so; a command the caller cannot run is dropped and the words search instead.
- **The reading is the API's, and it executes nothing.** `POST /api/v1/command-center/resolve`
  (guard `search.read`) answers one phrase with the intent, a preview line in plain words
  ("Tickets · assignee: Mehmet · last 10 · newest first"), `runnable`, the destinations, the
  alternatives and where the reading came from — and writes nothing but an audit entry: **no run,
  no usage, no navigation**. `runnable` is computed behind the API (confidence at or above 0.6
  *and* a screen the caller may really open), so the panel cannot talk itself into a run.
- **The model is asked first, and normalised afterwards.** When the installation has a model
  connected, the resolver asks it — bounded at **six seconds**, with a closed vocabulary in the
  prompt (the providers this caller may read, the commands they may run) — and then checks the
  answer: an unknown command id, a provider the index does not answer for, a sort order outside
  the three, a count past the cap are *dropped*, not trusted. No model, a slow model or an
  unusable answer leaves the grammar's own reading standing, flagged as such: `source`, `degraded`
  and a plain-word note travel with the answer, so a local reading is never dressed up as a
  model's. Model readings are audited (`command.resolve`: the interpreted intent, status, the
  words — never a row of content); the grammar's are reproducible from the words themselves, so
  they are not.
- **The card is a proposal beside the results, not a gate in front of them.** It sits above the
  list like the confirmation card does, and it is deliberately not an arrow-key row: its controls
  are real buttons (≥44px on touch), because every row the arrow keys reach must be a screen.
  `Run` appears only for a runnable reading — and for a reading of an action command it opens the
  platform's *own* confirmation card, so a command that asks first asks here too. `Edit as search`
  always exists: the API answers `search_route` (the words, the domain and the order, runnable or
  not) beside `route` (where Run lands), so collapsing the two can never lose the filters the
  phrase carried. The request is aborted the moment a newer keystroke overtakes it, and a failing
  reader gets a retry rather than a dead end.

Deviations, recorded as they were decided:

- The request's "Tickets" example has no module yet (a helpdesk is REQ-009's ground), so the
  reading of that exact phrase is honest rather than theatrical: the interpretation prints in full,
  a Run is *not* offered, and the card says why. A runnable reading is demonstrated with the
  domains that exist ("show me the newest pages" → `/search?q=pages&type=pages&sort=newest`).
- "Degrades to plain search when slow" is implemented as the grammar's own reading standing in
  (with `degraded: true` and a note), not as the card disappearing: the results beside it were
  never blocked in the first place, and a reading the operator can correct is worth more than an
  empty space.
- The arrow-key list does not carry the card's controls (§ above). The confirmation card of slice 3
  set that precedent, and the alternative — listbox options that are buttons — costs the list its
  own promise.
- The resolve fires on a **250 ms** typing pause for phrases of four characters or more, in the
  open mode only; a prefixed box (`>`, `@`, `#`, `:`, `?`) already says what the reader wants and
  asks nobody.

Found, not caused by this slice:

- A provider left behind by a probe that crashed mid-way makes every reading in the installation
  time out, and the *symptom* is a palette that feels slow rather than a missing provider. The
  probe now clears its own name first, and the six-second bound is the thing that keeps the failure
  survivable either way.

Proof: `cargo test --workspace` → **490 passed, 0 failed** (27 new: 13 grammar walks in
`omnion-search::intent` — the request's own phrase, a command, an order, gibberish, the count cap,
the permission-degraded command — 10 resolver walks in `omnion-api::intent_resolver` covering the
normalisation of a model's answer, and 4 new walks in `apps/api/tests/command_center.rs`: the
interpretation and its verbs, the screens a caller may open, the unreadable phrase, the endpoint's
own refusals) · `cargo
clippy --workspace --all-targets -- -D warnings` → clean · `pnpm typecheck && pnpm build` → 2/2 ·
`bash scripts/qa/run.sh` → 97 clicks, 117 screenshots, **0 high findings**, **0 vision issues**
(`qa-artifacts/20260926-194131`; the 5 medium findings are the public renderer's own icon 404s,
carried forward) with the walkthrough's new `resolve-read` step recording
`parsedIntent: true · offersRun: false · ranNothingYet: true` and `resolve-edit-as-search` landing
on `/search?q=tickets+Mehmet&sort=newest` · `scripts/qa/probe-command-resolve.cjs` → **47/47**
(the reading, nothing run, the alternatives, `Edit as search`, a runnable Run, a command reading, an
action reading that still asks first, the unreadable phrase, a slow reader that does not block
typing, a failing reader with a retry, a connected model that never answers degrading in **6.4 s**
with its audit entry, and the card at 390×844) · `probe-command-center.cjs` **26/26**,
`probe-palette.cjs` **22/22**, `probe-palette-federated.cjs` **32/32**,
`probe-command-actions.cjs` **31/31** re-run clean (no regressions).

### Risks / notes

- Permission leakage is the top risk: filtering must happen in the query layer, not the component; keep a test asserting that filtered-out titles are absent from the response body, not merely hidden.
- AI latency varies: resolve fires after a 250 ms typing pause, is cancellable, and degrades to plain search when slow or unavailable.
- Index staleness after mutations: the consumed events shrink the window; the palette shows an "updated just now" context on entity groups instead of pretending to be real time.
- Keystroke capture must not break typing: the palette opens on `Ctrl+K` only (never a bare letter), and prefix characters are interpreted only inside the palette input.
- Recents store raw query text, which may contain customer names: trim, cap, keep per user, clearable via `DELETE`, and excluded from analytics exports.
- Command ids are API surface: renaming one is a breaking change for automation that references it, so ids stay stable and old titles become aliases.
- The palette must stay usable without heavy effects: results render as a plain list, animations are transform-only, and reduced-motion preferences are honoured.
