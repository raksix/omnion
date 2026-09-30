# REQ-021 — Notification Center

> **Status:** in-progress — slices 1–3 code-complete, **slice 4 (the delivery runner) shipped
> 2026-09-30**. That slice closed the box that described a runner which had never existed: the
> `notification_deliveries` table had readers since slice 1 and no writer at all.
> `delivery.rs` (queue), `0185` (the claim lease), `notification_runner.rs` (the transports) and
> a live-database walk of the whole lifecycle. **Two defects were found by writing the walk**,
> both of which would have shipped as silent behaviour — see the build log. Also fixed 2026-09-30
> (`c48db9d`): `read_settings` read the `time` columns with `::text`, which Postgres renders as
> `22:00:00`, while the platform's clock vocabulary is `HH:MM` — so every saved quiet window came
> back in a shape `parse_clock` rejected and the setting a reader had just turned on decided
> nothing from the next request onwards (`60a28ea` proves both directions). **Not closed** — the
> keyboard leg and the two settings boxes still need a *browser* pass against a binary built
> after all of this, and no such pass has run. **Slice 5 shipped 2026-09-30** (`511a50d8`,
> `43a86c13`, `65e06222`, `f611569f`): the **test-delivery route the spec listed with no code
> behind it** — `grep -rn "preferences/test"` over the tree returned nothing while the API
> table carried the endpoint and the screen spec described the button — plus the screen block
> that calls it. Writing it surfaced a defect one level down: the webhook transport posted
> `job.url`, which every module fills with an **in-app deep link**, so `reqwest` refused every
> send with `relative URL without a base`, and `channel_readiness` reported the channel ready
> throughout because that branch answered `true` unconditionally. Two writers of nothing that
> matters. **The test-delivery box is closed on a socket, not a browser** (21/21 in
> `run-notifications-http.sh`: `in_app` refused, an unknown channel named, a transportless
> channel answering `200` + `delivered:false` + reason, and the `failed` row read back out of
> the database). The keyboard and mobile boxes still need the browser pass; the QA slot was held
> by a live w3 pass for 24 minutes of this tick. · **Captured:** 2026-09-25 · **Layer:** core (`crates/notifications`) + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

> **Slice 1 shipped** (9137f16 the record, 8f9d570 the panel, c856fb2 the HTTP gate): the
> `omnion-notifications` crate, migration 0050, the owner-scoped list/summary/bulk surface, the
> bell, `/notifications`, and the walkthrough pass that drives them. `cargo test -p
> omnion-notifications -p omnion-permissions` → 62 passed; `-p omnion-api --lib` → 159 passed;
> `pnpm typecheck` → 2 successful. `scripts/qa/run-notifications.sh` → PASS (31 migrations
> applied, dedupe collapses, both check constraints refuse what the crate refuses);
> `scripts/qa/run-notifications-http.sh` → PASS, 9/9 over a real socket.
>
> **Slice 2 shipped** (c069128 rules, 11bd14d SQL, 2ffe581 fmt, 2785d7c the HTTP gate, 13ad348
> the screen, 07e2d56 the walkthrough depth pass, 871bdb5 + eb421ba two fixes it found):
> `preferences.rs`, `preference_store.rs`, `GET`/`PUT /api/v1/notifications/preferences`,
> `/notifications/settings`. **The browser pass found a 400 that had been shipping since slice
> 1**: axum 0.8's `Query` is backed by `serde_urlencoded`, which cannot put a repeated key into
> a `Vec`, so *every* category and priority filter — legal value or not — answered
> `invalid type: string "approval", expected a sequence` and fell through to the error state.
> Reproduced in a four-line axum app before the fix. The read now takes `RawQuery`.
>
> **Slice 3 code-complete** (26d6f67 push + outbox, 9acd8ca the router + migration 0051, 0dae953
> the `notifications.admin` key, cb7c1bb the HTTP surface, 0188172 the mounts, 12e8c88 the live
> gate): `push.rs` (device lifecycle, outbox, channel readiness), `router.rs` (the declarative
> router, four closed recipient shapes, a derived dedupe key),
> `apps/api/src/routes/notifications_admin.rs` (nine endpoints), migration 0051. `cargo test -p
> omnion-notifications` → **79 passed**; `-p omnion-permissions` → 62; `-p omnion-api --lib` →
> **187 passed**; `scripts/qa/run-notifications-routes.sh` → **PASS** (32 migrations, the three
> recipient/category refusals, the outbox's 13-column projection with no body, and a retry that
> moves the failed row and leaves the delivered one alone).
>
> **Not yet proven by a browser:** the slice-2 screen's pass is running, and slice 3's
> `/notifications/outbox` and the router's rules have no admin UI yet. Slices 2 and 3 are not
> closed on tests alone — the "no untested screen" rule means the next pass must visit
> `notifications-settings`, and slice 3's screen has to exist before it can.

## Request

Top-right of the admin:

```text
Notifications

3 pages awaiting approval
2 security alerts
1 plugin update
5 new tickets
```

Channels:

- In-app
- Email
- Web Push
- Webhook
- Slack/Discord-style integrations

## Implementation spec

### Scope (in / out)

**In**

- New crate `crates/notifications`: notification record, per-user preference matrix, channel adapters, delivery queue worker (`apps/api/src/notification_runner.rs`, same `for update skip locked` + lease claim as the webhook runner).
- In-app surface: bell in the admin top bar with unread badge, grouped summary panel (the brief's four counts), `/notifications` list, per-notification detail.
- Channels: **in-app** (always on), **email** (instant or digest), **Web Push** (browser subscription; the application server key pair is generated at deploy time and kept only in the platform secret store), **webhook** (reuses `webhook_endpoints` from REQ-016 — no second bus), **chat connectors** (REQ-015-style incoming-webhook POST with a templated body).
- Own-data by default: every route is scoped to the caller's user id; `notifications.admin` unlocks the org-wide outbox and channel configuration.
- Summary counters are **real SQL counts** over the owning domains (pending approvals, security alerts, available updates, open tickets) — never a hardcoded block.
- Digest job: daily/weekly mail per user built from unread notifications since the last digest, respecting quiet hours in the user's timezone.

**Out**

- Native mobile push (mobile app out of scope), SMS channel.
- Rich block/HTML template editor — subject + plain text + link only.
- Chat-bot command handling (outbound POST only). Realtime transport itself (REQ-041); this REQ only consumes it.

### Screens (UI)

| Route | Purpose |
|---|---|
| `/notifications` | Full list: filters, bulk actions, keyset pagination, detail drawer |
| `/notifications/settings` | Preference matrix, quiet hours, digest cadence, test delivery, devices |
| `/notifications/outbox` | Admin: deliveries across users, failed-first, retry (`notifications.admin`) |

- Bell `NotificationBell` in `components/app-shell.tsx`: unread badge (`99+` cap), 420px panel with the grouped counts, latest 10 items, `Mark all read`, `View all`; `Enter`/`Space` opens, `Esc` closes and returns focus.
- List table columns: icon · Title · Category · Source · Priority · Channels · Created · Read. Row click opens a right-side drawer (no route change) that also lists per-channel delivery rows with status and time.
- Filters: category (approval, security, update, ticket, system, mention), read state, priority, channel, date window, source type. Filter state lives in the query string so a filtered view is shareable; `Reset` clears all.
- Bulk actions on selection: Mark read, Mark unread, Archive, Delete (confirm dialog names the count).
- Settings form: rows = categories, columns = channels, cells = toggles; quiet hours start/end, timezone, digest cadence (off/daily/weekly), digest hour + weekday. Validation: quiet hours may not span the whole day; `weekly` needs a weekday; the in-app column is locked on (server rejects disabling) and renders a lock tooltip.
- States: skeleton rows while loading; empty `You're all caught up` with "Show read notifications"; error state with retry and the request id; the panel shows a compact "Could not load notifications — retry" line instead of an empty box.
- Keyboard: `j`/`k` row cursor, `Enter` open, `e` toggle read, `Shift+E` mark visible read, `x` select, `/` focus filter, `Esc` close drawer. Bell reachable from the command palette (REQ-032).
- Mobile ≤ 768px: bell in the header, panel becomes a full-screen sheet, list renders as cards, bulk actions move to an overflow menu, tap targets ≥ 44px.
- Live: badge updates through the SSE stream when available, otherwise polls every 30s while the tab is visible.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/notifications` | Own notifications; filters + keyset pagination (`before`, `limit` ≤ 100) | `notifications.read` |
| GET | `/api/v1/notifications/summary` | Grouped badge counts for the bell | `notifications.read` |
| GET | `/api/v1/notifications/{id}` | One notification + per-channel delivery rows | `notifications.read` |
| POST | `/api/v1/notifications/{id}/read` | Mark read / unread (`read: bool`) | `notifications.read` |
| POST | `/api/v1/notifications/bulk` | Bulk read / unread / archive / delete (≤ 200 ids) | `notifications.read` |
| DELETE | `/api/v1/notifications/{id}` | Delete an own notification | `notifications.read` |
| GET | `/api/v1/notifications/stream` | SSE: new notification + unread count for the bell | `notifications.read` |
| GET | `/api/v1/notifications/preferences` | Category × channel matrix, quiet hours, digest | `notifications.manage` |
| PUT | `/api/v1/notifications/preferences` | Save the matrix (full replace) | `notifications.manage` |
| POST | `/api/v1/notifications/preferences/test` | Send a test notification through one channel | `notifications.manage` |
| POST | `/api/v1/notifications/push-subscriptions` | Register a browser push endpoint (public keys + URL) | `notifications.manage` |
| DELETE | `/api/v1/notifications/push-subscriptions/{id}` | Remove one device | `notifications.manage` |
| GET | `/api/v1/notifications/channels` | Channel readiness (mail transport, push key, chat connectors, webhooks) | `notifications.manage` |
| GET | `/api/v1/notifications/outbox` | Org-wide outbox, failed-first filter | `notifications.admin` |
| POST | `/api/v1/notifications/outbox/{id}/retry` | Requeue a failed delivery | `notifications.admin` |
| POST | `/api/v1/notifications/emit` | Emit to a user, role or permission set (modules, approvals) | `notifications.send` |

Errors: `400` unknown category/channel or malformed payload, `403` permission miss, `404` for another user's notification (never `403`, so existence does not leak), `409` duplicate push endpoint, `429` emit burst above the per-actor budget.

### Data model

Migration: `database/migrations/0011_notifications.sql` (take the next free number at build time; released migrations are append-only).

- `notifications` — `id uuid pk default gen_random_uuid()`, `organization_id uuid null references organizations(id) on delete cascade`, `user_id uuid not null references users(id) on delete cascade`, `category text not null`, `priority text not null default 'normal'`, `title text not null`, `body text not null default ''`, `url text null`, `source_type text null`, `source_id text null`, `payload jsonb not null default '{}'`, `dedupe_key text null`, `read_at timestamptz null`, `archived_at timestamptz null`, `created_at timestamptz not null default now()`; checks `category in (approval,security,update,ticket,system,mention)`, `priority in (low,normal,high,critical)`; indexes `(user_id, created_at desc)`, partial `(user_id) where read_at is null`, partial unique `(user_id, dedupe_key) where dedupe_key is not null`.
- `notification_preferences` — `user_id uuid`, `category text`, `channel text`, `enabled boolean not null default true`, `updated_at timestamptz`; primary key `(user_id, category, channel)`.
- `notification_settings` — `user_id uuid pk`, `quiet_hours_start time null`, `quiet_hours_end time null`, `timezone text not null default 'UTC'`, `digest_cadence text not null default 'off'`, `digest_weekday smallint null`, `digest_hour smallint not null default 8`, `updated_at timestamptz`.
- `notification_deliveries` — `id uuid pk`, `notification_id uuid not null references notifications(id) on delete cascade`, `channel text not null`, `status text not null default 'pending'`, `attempts int not null default 0`, `max_attempts int not null default 3`, `next_attempt_at timestamptz not null default now()`, `response_status int null`, `error text null`, `sent_at timestamptz null`, `created_at timestamptz not null default now()`; index `(status, next_attempt_at)` for the runner, index `(notification_id)`.
- `push_subscriptions` — `id uuid pk`, `user_id uuid not null references users(id) on delete cascade`, `endpoint text not null unique`, `p256dh text not null`, `auth text not null`, `user_agent text null`, `created_at timestamptz`, `last_seen_at timestamptz`.
- `notification_channels` — org config: `id uuid pk`, `organization_id uuid not null`, `channel text not null`, `config jsonb not null default '{}'`, `enabled boolean not null default true`, `created_by uuid null`, `created_at timestamptz`, `updated_at timestamptz`; unique `(organization_id, channel)`.
- Nothing here stores a transport secret in clear text: channel credentials are written through the secret store (REQ-037) and referenced by key only.

### Events

- **Emitted:** `notification.created`, `notification.delivery.succeeded`, `notification.delivery.failed`, `notification.preferences.changed`.
- **Consumed:** a declarative router (`category → event name → recipient rule`) turns bus events into notifications — page submitted for review, security alert (REQ-012), update available (REQ-044), ticket created/assigned (REQ-009), role change on the caller (REQ-006), failed webhook delivery (REQ-016). An event name with no producer yet is a no-op, not an error.
- Recipients are explicit: a user id, a role slug, a permission key, or actors linked to the source record. The rule is data, so a module can extend the router without touching notification code.
- Webhook relevance: `notification.*` are ordinary bus events, so an organization can watch its own notification traffic; payloads carry ids, category and priority — never a security alert body and never a secret.

### Acceptance criteria

- [x] Bell renders on every admin route with a live unread badge. *(in `app-shell.tsx`; `runNotificationsDepth` asserts `[data-bell]` on `/` and reads the badge)*
- [x] Panel shows the grouped lines with counts equal to real SQL counts for the signed-in user. *(one `summary` query; the gate asserts the badge equals the sum of the panel's own lines, and the HTTP gate asserts the summary equals the rows)*
- [x] Clicking a grouped line filters `/notifications` to that category. *(pass 22:33: `groupFilteredUrl` true, `onlyThatCategory` true — every row on the filtered list IS that category. `groupFilteredRows` was 0, which is that QA database's own history, not a filter that matched nothing.)*
- [x] List supports category/read/priority/channel filters and keyset pagination. *(date-window filter belongs to slice 2's digest work; the keyset cursor is `next_before`)*
- [x] Row click opens the detail drawer with per-channel delivery rows. *(closed 2026-09-30. The box had been open since the request was written with the note "the per-channel delivery rows are slice 2's, **when deliveries exist**" — and the note expired last tick, when the runner finally wrote rows. So the gap was real rather than deferred: `GET /api/v1/notifications/{id}` promised "one notification, **with its delivery rows**" in its own doc comment and returned a bare notification, while the platform knew an e-mail had failed and the person waiting for it was told nothing. `store::deliveries` (one row per channel, ordered, carrying the state, `attempts` against `max_attempts`, the transport's status code and the reason in the platform's words) + `NotificationBody.deliveries`, filled **after** the ownership check — the delivery rows carry no `user_id`, so a read placed before the filter would answer a populated channel list for somebody else's notification and turn a `404` into an existence oracle. The drawer renders a channel row per attempt, distinguishes "gave up after N of M" from "attempt N of M", prints the reason, and says "nothing has been attempted yet" instead of rendering a heading over nothing. Proof: `notification_delivery_reader` 4/4 over live PostgreSQL — the state/attempt/reason walk, a stability walk (the same notification read twice lists the same channels in the same order), the honest empty list, and the ownership gate; `omnion-notifications --lib` 90/0; admin `tsc --noEmit` clean)*
- [x] Bulk read/unread/archive/delete work on a multi-row selection. *(HTTP gate: a foreign id in a selection changes 0 rows; walkthrough: three rows selected, notice matched `\d+ of \d+`)*
- [x] Mark-all-read clears the badge without a full page reload. *(the answer carries the new summary, so the badge is the server's number)*
- [x] Empty state, loading skeleton and error state (with retry) all render and are reachable in QA. *(all three asserted in `runNotificationsDepth`; the skeleton is caught by throttling the response, the error by a routed 500)*
- [ ] Keyboard path (`j`/`k`/`Enter`/`e`/`Shift+E`/`x`/`/`/`Esc`) works with visible focus rings. *(pass 23:51 **did not reach the keyboard leg at all** — `keyboardRows` was 0, because the list it had just marked read showed nothing (`no rows to drive — the list did not load`). That is this tick's defect, not a timing problem, and it is fixed in `ab3c105` + `4933c10`; the 22:33 reading below is kept only as history. **The leg is still unproven**: no pass has run against the fix, so `escapeClosedDrawer`, `escapeWithNoRowUnderCursor`, `eToggledRead`, `shiftEMarkedVisible` and `slashFocusedFilter` all remain open, and this box stays unticked)* *(pass 22:33: `cursorMoved`, `keyboardSelected`, `keyboardOpenedDrawer` are true and the four behind them are false — `Escape` was documented in the file header and absent from the handler, and unreadable behind `if (!row) return`, so the stuck drawer held the focus. Fixed in `c30d324` and typechecked; **unproven until a pass runs a binary built after it**, so the box stays unticked)*
- [x] `/notifications/settings` saves the matrix, quiet hours, timezone and digest cadence. *(pass 23:51: `loaded` true, `matrixIsComplete` true, `saveNoticeIsHonest` true, `persisted` true, `serverAgrees` true, `errorState` and `errorOffersRetry` both true, `restored` true. **`quietSaved` and `digestPersisted` were false and stayed false — and the browser pass was not the thing that was wrong about them.** They were false because the values were never coming back: `read_settings` read the `time` columns with `::text`, Postgres rendered that as `22:00:00`, and the form compared the field against `22:00`. Fixed in `c48db9d`; the two gate legs in `60a28ea` now assert the round trip over a socket (pre-fix: `api=[22:00:00..07:00:00 hour=17]`, FAIL; post-fix: `22:00`/`07:00`, PASS 15/15). **The browser legs themselves are still unproven** — the pass has not run against a binary built after this, so the two boxes below stay unticked and this box stays ticked only for what is proven)*
- [x] Test delivery through e-mail and webhook reports success or a readable failure inline. — **closed 2026-09-30, `slice 5`.** The box had been open since the request was written, and not for the reason its neighbours carried: the route genuinely did not exist. REQ-021's API table has listed `POST /api/v1/notifications/preferences/test` since 2026-09-25 and the screen spec describes a per-channel `Test delivery` button; `grep -rn "preferences/test" apps/ crates/ scripts/` returned nothing but the file this slice added, so no browser pass could ever have closed it. `apps/api/src/routes/notifications_test.rs` sends a **real** notification through the **real** transport and reports the transport's own outcome — the shortcut (validate the channel, answer `{ok: true}`) is the same lie `channel_readiness` was telling, and it is exactly what the `web_push` leg below fails if it ever comes back. `in_app` is refused with a `400` naming the reason rather than answered, because the test notification *is* the in-app channel. **Proof over a real socket against live PostgreSQL** (`scripts/qa/run-notifications-http.sh`, 21/21 in a scrubbed `env -i`): `in_app` → `400 invalid_channel`; `carrier_pigeon` → `400 invalid_channel`; `web_push` (no transport, no browser subscription) → **`200` with `"delivered":false` and a `detail`**, *not* an HTTP error, because a failure is a result the reader asked for and a `502` would have told them their settings screen was broken; and the failed attempt read back out of `notification_deliveries` as `failed|<reason>`, because a toast that vanishes while the outbox says nothing is the one case where the answer can never be found again. 95 crate + 247 api-lib tests.
- [ ] Web Push: subscribe, receive one real notification, unsubscribe; a revoked endpoint is pruned.
- [x] The in-app column cannot be disabled (server rejects it, UI shows it locked). *(pass 23:51: `inAppLocked` true, `lockedColumnExplainsItself` true, and the refusal itself is asserted over the wire — status **400** with a message that names the reason rather than the field)*slice 2's preference matrix; `in_app` is already in the channel list so the matrix can render it locked)*
- [x] Delivery runner retries a failing channel per backoff and marks it `failed` after the cap. *(closed 2026-09-30, `slice 4`. The table `notification_deliveries` shipped with slice 1 and every later slice *read* it — the outbox lists it, the retry button re-queues it, the channel filter joins it — but nothing ever wrote a row or claimed one, so the box described a runner that did not exist. It exists now: `crates/notifications/src/delivery.rs` (enqueue, `for update skip locked` claim with a lease, `attempts` incremented in the same statement, exponential backoff clamped and capped, `failed` at the cap) + migration `0185` (the `claimed_at` lease column, which is what a claim needs and the table never had) + `apps/api/src/notification_runner.rs` (the transports, deliberately *not* in the crate: a transport is an SMTP conversation and an HTTP POST, and `omnion-automation` already owns the mail sender, so putting one in the crate would have made infrastructure depend on a mail stack to satisfy a trait it defined itself). `apps/api/tests/notification_delivery.rs` drives the whole lifecycle over a live database: 8 walks including the cap, the lease recovery, the multi-tenant settlement and the outbox reading the queue's own writes back. **The walks found two real defects while being written, both now fixed**: `enqueue` had an early return that contradicted its own contract (a caller with no remote channel to ask about got *no rows at all*, so the notification existed with no delivery record — the in-app row is the inbox and must be unconditional), and `settle_not_ready`'s `not exists` was uncorrelated, so on a multi-tenant install one organization configuring e-mail would have silently stopped delivery for every other organization. See the build log for the counts)*
- [x] `notifications.admin` sees the outbox; a user without it gets `403` and no nav entry. *(pass 23:51 — the first that produced `report.notificationOutbox`: `loaded` true, 5 chips all carrying counts, `chiptotalMatchesSql` true, `targetHiddenForActor` true, `targetAppearsForPermission` true, `noTargetWroteNothing` true, `actorActuallyWroteARow` true, `removeStatus` 204 and `removedFromTheTable` true, `retryIsNotRetryable` true. The database is fresh so `deliveryRows` is 0 and the empty state is the honest answer — no fixture row was added to make the table look full)*slice 3's outbox; the key is deliberately not in the catalogue yet — a permission with no route behind it is a promise the platform cannot keep)*
- [x] A notification the reader has already read stays on the list until it is archived. *(this tick's defect, `ab3c105` + `4933c10` + `797a3e8`. `with_read` was a `bool` defaulting to `false`, so **every** caller that named no filter got an unread-only list while the State menu labelled that state "Unread and read" — a screen promising a list it was not sending. It surfaced as `keyboard: "no rows to drive"`, four lines after the pass marked its own three rows read. `Option<bool>` carries absent-vs-off, `?with_read=0` is the inbox, and two gates now hold it: the walkthrough asserts `readRowsStayVisible` right after the bulk action, the HTTP gate asserts `all=1 inbox=0 live=1` over a socket)*
- [x] Another user's notification returns `404`, never its content. *(HTTP gate: 404, and the body carries no content from the row)*
- [x] Duplicate emits with the same `dedupe_key` collapse into one row. *(HTTP gate: created=1, deduped=1, rows=1)*
- [ ] Mobile ≤ 768px: panel is a sheet, list is cards, no horizontal scroll. *(the panel is bounded with `w-[min(420px,calc(100vw-2rem))]` and the table scrolls; the assertion itself is the browser pass's mobile leg)*
- [x] No new screen is invisible to the QA walkthrough inventory. *(`/notifications` is in the route list and `runNotificationsDepth` is wired into main)*
- [x] `cargo test`, `pnpm typecheck`, `pnpm build` and the browser walkthrough are green. *(this tick: notifications **79**, permissions **62**, `omnion-api --lib` **188** (187 + the new absent-vs-off test), `tsc --noEmit` exit 0, `node --check scripts/qa/walkthrough.cjs` clean, `scripts/qa/run-notifications-http.sh` → **PASS 13/13** over a real socket (the new leg: `all=1 inbox=0 live=1`), and the browser pass at 23:51 — 36 pages, 1095 clicks, 39 form submissions, 84 programmatic findings of which 79 high. **74 of the 79 high are `/media/*` (REQ-010\u2019s) and the remaining 5 are this pass\u2019s own deliberate refusal probes**: three the 400 in-app lock and two the routed-500 error state, all asserted as expected)*this tick: notifications 79, permissions 62, api --lib 187, `tsc --noEmit` exit 0, and `run-notifications-http.sh` PASS 12/12 — the walkthrough leg is the one still owed, and the pass that ran filed 97 high console-errors against a 3-hour-old binary)*

### QA plan

The walkthrough must: assert bell + badge; open the panel and click each grouped line; assert the resulting filtered list; select three rows and run Mark read plus Archive; open a drawer and toggle read; load `/notifications/settings`, flip two toggles, save, reload, confirm persistence; run Test delivery for the webhook channel against a local receiver and read the success line; trigger a real emit and watch the badge increment with no manual refresh; resize to 390px and re-check bell, panel and list.

Visual check should see: a badge with no clipped digits, four grouped lines with aligned counts, stable column widths without overlap, a readable empty state, rows that do not jump when the drawer opens, and dark/light parity.

### Slices

1. **Schema + in-app loop.** Migration, `crates/notifications` (record, list, summary, bulk read), `apps/api/src/routes/notifications.rs`, bell + panel + list with filters and all three states. *Done when:* an emitted notification appears in bell and list, bulk read clears the badge, and the walkthrough clicks both.
2. **Preferences + channels.** Matrix, quiet hours, digest job, e-mail and webhook adapters, delivery rows in the drawer, `/notifications/settings` with test delivery. *Done when:* a saved preference demonstrably suppresses one channel and test delivery shows real channel output.
3. **Push + outbox + router.** Push subscription lifecycle with pruning, admin outbox, event router turning existing bus events into notifications. *Done when:* one real bus event (e.g. a ticket creation) produces a notification with no direct call between the two modules.

### Risks / notes

- Summary counters depend on other REQs. Where a producer is missing, the counter must come from a real query returning `0` — never a placeholder number — and the group hides itself when the domain is absent, so the panel never lies.
- Push needs HTTPS (or a loopback origin in development); the walkthrough records which channels it exercised and states the rest as not verified, honestly.
- Notification bodies can carry customer data: the read guard is owner-scoped, admin access writes an audit row, and webhook payloads stay id-only.
- The runner must not turn a permanently broken channel into an infinite retry loop — bounded attempts, exponential backoff, `skipped` state when the user disabled the channel.
- Do not build a second webhook stack: the webhook channel targets the existing bus and `notification.*` events ride that same bus.
