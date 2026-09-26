# REQ-021 — Notification Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/notifications`) + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

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

- [ ] Bell renders on every admin route with a live unread badge.
- [ ] Panel shows the four grouped lines with counts equal to real SQL counts for the signed-in user.
- [ ] Clicking a grouped line filters `/notifications` to that category.
- [ ] List supports category/read/priority/channel/date filters and keyset pagination.
- [ ] Row click opens the detail drawer with per-channel delivery rows.
- [ ] Bulk read/unread/archive/delete work on a multi-row selection and survive a reload.
- [ ] Mark-all-read clears the badge without a full page reload.
- [ ] Empty state, loading skeleton and error state (with retry) all render and are reachable in QA.
- [ ] Keyboard path (`j`/`k`/`Enter`/`e`/`Shift+E`/`x`/`/`/`Esc`) works with visible focus rings.
- [ ] `/notifications/settings` saves the matrix, quiet hours, timezone and digest cadence.
- [ ] Test delivery through e-mail and webhook reports success or a readable failure inline.
- [ ] Web Push: subscribe, receive one real notification, unsubscribe; a revoked endpoint is pruned.
- [ ] The in-app column cannot be disabled (server rejects it, UI shows it locked).
- [ ] Delivery runner retries a failing channel per backoff and marks it `failed` after the cap.
- [ ] `notifications.admin` sees the outbox; a user without it gets `403` and no nav entry.
- [ ] Another user's notification returns `404`, never its content.
- [ ] Duplicate emits with the same `dedupe_key` collapse into one row.
- [ ] Mobile ≤ 768px: panel is a sheet, list is cards, no horizontal scroll.
- [ ] No new screen is invisible to the QA walkthrough inventory.
- [ ] `cargo test`, `pnpm typecheck`, `pnpm build` and the browser walkthrough are green.

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
