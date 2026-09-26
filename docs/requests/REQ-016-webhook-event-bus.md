# REQ-016 — Webhook + Event Bus

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/webhooks`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Every important occurrence becomes an event:

```text
page.created
page.updated
page.published
user.created
user.deleted
order.created
plugin.installed
theme.activated
```

Plugins can subscribe to these events.

## Notes

- Expands the event system sketched in docs/01-VISION.md §13.

## Implementation spec

The plumbing already exists: `crates/events` (bus, signature, sender, engine), migration `0009_events_webhooks.sql`, and `apps/api/src/routes/webhooks.rs` with
endpoint CRUD, deliveries and `/api/v1/events`.
This request closes the gap between "the bus exists" and "every important occurrence is on the bus, and an operator can see and repair what happened".

### Scope (in / out)

**In**

- **Emission coverage.** One shared Rust registry (`omnion_events::catalogue`) lists every event name, its area, its human description and its payload fields;
  every mutating module records its events through it. Minimum coverage for this request: `page.created|updated|published| unpublished|deleted|restored`,
  `translation.updated|published`, `media.uploaded|updated| deleted`, `user.created|updated|deleted`, `site.created|updated|archived`, `domain.added| verified|removed`,
  `plugin.installed|activated|deactivated|uninstalled`, `theme.activated`, `workflow.run.started|completed|failed`, `webhook.delivery.failed`.
  `order.created` arrives with the commerce module and is listed as reserved.
- **Subscriptions.** Per-endpoint event selection from the catalogue, including group wildcards (`page.*`, `media.*`) stored expanded and re-expanded when the
  catalogue grows (a new event in a subscribed group is delivered without touching the endpoint).
- **Delivery operations.** Queue history per endpoint with filters, single and bulk redelivery, test delivery, secret rotation, delivery stats, and an
  operator-visible reason for every failed attempt.
- **Event feed.** `/events` in the panel: newest-first, payload inspector, filters, copy helpers, retention sweep (default 30 days, configurable per
  organization).

**Out**

- Inbound webhooks and connector recipients (REQ-015 Integration Hub).
- Consumer SDKs, cross-region delivery relays (REQ-035), broker fan-out (Kafka-style).
- Payload versioning beyond additive-only evolution (policy documented, not implemented).
- Global ordering guarantees — ordering is per endpoint, by `events.id`.

### Screens (UI)

- **`/webhooks` — endpoints.** Table columns: Name, Receiver host (URL truncated to host, full URL on hover), Events (count + first two chips), Status
  (enabled/disabled badge), Last delivery (relative time + status dot), 24 h success rate, Created, row actions (Open, Test, Disable, Delete). Filters: text
  search over name/URL, status (all/enabled/disabled), event name (multi-select). Bulk actions: Enable, Disable, Delete (typed confirmation). Primary button `New endpoint`
  → `/webhooks/new`. Keyboard:
  `/` focuses search, `n` opens the create form, `j`/`k` move the row cursor, `Enter` opens the row, `Esc` clears filters.
- **`/webhooks/new` and `/webhooks/[id]/edit` — endpoint form.** Fields: Name (1–64 chars, unique per organization case-insensitively, validated inline), URL
  (`http(s)://`, no whitespace; a non-HTTPS warning badge appears but does not block), Events (grouped multi-select from the catalogue, group select-all, 1–32
  selections counted live), Secret (radio *Generate for me* / *Provide my own*; own secret 16–128 chars, no whitespace), Enabled toggle. Create responds with the
  secret exactly once: a copy field, a `I stored it` checkbox, and a `Done` button disabled until it is ticked.
  Edit never shows the secret, only `Rotate secret`.
- **`/webhooks/[id]` — endpoint detail.** Tabs: Overview (settings summary, Test delivery, Rotate secret, Disable, Delete), Deliveries, Stats. Deliveries table
  columns: Delivery id (short form + copy), Event name, Status badge, Attempts (`2/5`), Response code, Duration, Next attempt, Created, actions (View, Redeliver).
  Filters: status, event name, window (24 h / 7 d / 30 d / custom), delivery-id search. Bulk: `Redeliver selected` (pending rows are skipped with a note).
  Row expansion shows the request headers (including the signature header name) and the decoded payload, plus `Copy as cURL`.
- **`/events` — event feed.** Table columns: Id, Name, Site, Actor, Payload (truncated preview), Recorded. Tabs: Feed, Catalogue. Catalogue lists event name,
  area, description, payload fields, delivery count in the last 24 h. Filters: name (multi-select), site, actor, window. Row expansion renders the payload as a
  collapsible JSON tree with copy-per-node. Empty state:
  "Nothing recorded yet — publish a page to see the first event."
- **States.** Loading = skeleton rows (existing `loading-table`); error = inline banner with the API error code and a retry button; `403` = "Your role cannot
  manage webhooks" with the missing permission name;
  empty endpoint list = illustration + `Connect your first endpoint` + a link to the signature documentation.
- **Mobile (< `lg`).** Tables become cards (name, host, status, last delivery), filters move into a bottom sheet, the secret-once dialog becomes a full-height
  sheet, tabs scroll horizontally. The sidebar drawer from `AppShell` is reused unchanged.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/webhooks` | List endpoints of the organization | `webhooks.read` |
| POST | `/api/v1/webhooks` | Create an endpoint; returns the secret once when generated | `webhooks.manage` |
| GET | `/api/v1/webhooks/{id}` | One endpoint | `webhooks.read` |
| PATCH | `/api/v1/webhooks/{id}` | Update name/URL/events/enabled | `webhooks.manage` |
| DELETE | `/api/v1/webhooks/{id}` | Remove the endpoint and its queue rows | `webhooks.manage` |
| POST | `/api/v1/webhooks/{id}/secret/rotate` | Replace the signing secret; returns it once | `webhooks.manage` |
| POST | `/api/v1/webhooks/{id}/test` | Queue one signed `webhook.test` delivery | `webhooks.manage` |
| GET | `/api/v1/webhooks/{id}/deliveries` | Delivery history; `status`, `name`, `from`, `to`, `cursor`, `limit` | `webhooks.read` |
| POST | `/api/v1/webhooks/{id}/deliveries/{delivery_id}/redeliver` | Requeue one delivery | `webhooks.manage` |
| POST | `/api/v1/webhooks/{id}/deliveries/redeliver` | Requeue many (`{delivery_ids: []}`, max 100) | `webhooks.manage` |
| GET | `/api/v1/webhooks/{id}/stats` | 24 h / 7 d success rate, p95 duration, failed count | `webhooks.read` |
| GET | `/api/v1/events` | Event feed; `name`, `site_id`, `actor_user_id`, `from`, `to`, `cursor`, `limit` | `events.read` |
| GET | `/api/v1/events/{id}` | One event with full payload | `events.read` |
| GET | `/api/v1/events/catalogue` | Names, areas, descriptions, payload fields | `events.read` |

All list reads answer the envelope `{"items": [...], "next_cursor": "..." | null}` (the current `{webhooks|deliveries|events: []}` shape is kept for one release
and marked deprecated in the OpenAPI document).

### Data model

Existing tables (migration `0009`): `events`, `webhook_endpoints`, `webhook_deliveries` (`attempts`, `max_attempts`, `next_attempt_at`, `claimed_at`,
`response_status`, `error`). Migration `0011_webhook_ops.sql` (number is a placeholder — renumber to the next free slot at build time):

- `webhook_deliveries` gains `trigger text not null default 'event' check (trigger in ('event','test','replay'))`, `replayed_at timestamptz`, `duration_ms integer check (duration_ms
>= 0)`, and `redeliver_count integer not null default 0 check (redeliver_count between 0 and 10)` — the existing unique index `(endpoint_id, event_id)` stays, so a redelivery **resets** the row (`status='pending'`, `attempts=0`, `next_attempt_at=now()`, `replayed_at=now()`, `redeliver_count
= redeliver_count + 1`) instead of inserting a second row.
- Indexes: `webhook_deliveries (status, created_at desc)`, `webhook_deliveries (endpoint_id, status, created_at desc)`, `events (site_id, id desc)`, `events (actor_user_id, id desc)`.
- Retention: no schema change; the sweeper deletes `events` older than the configured window and cascades to `webhook_deliveries`.
- No secret is ever stored in plaintext logs; `webhook_endpoints.secret` keeps the current write-only behaviour (shown once, rotations replace it silently).

### Events

- **Emitted:** `webhook.endpoint.created|updated|removed|tested` (audit + bus), `webhook.delivery.failed` (attempts exhausted, includes endpoint name, event
  name, last response code, attempt count), `webhook.secret.rotated` (payload carries no secret material).
  The first three are also delivered to endpoints that subscribe to the `webhook.*` group.
- **Loop guard:** a delivery that carries a `webhook.*` event never emits its own `webhook.delivery.failed` — failure of a webhook-management delivery is
  recorded on the bus and shown in the feed, one level deep only.
- **Consumed:** every module event listed in Scope; the bus resolves organization from the emitter, never from the payload.
- **Webhook relevance:** this request *is* the webhook surface. `webhook.delivery.failed` is the signal REQ-021 (notification centre) turns into an operator
  alert.

### Acceptance criteria

- [ ] `GET /api/v1/events/catalogue` returns every event name in the registry with area, description and payload fields, and the `/events` Catalogue tab renders
  it.
- [ ] Publishing a page records `page.published` with `page_id`, `site_id`, `revision_no` and `slug` in the payload; creating, updating, unpublishing and
  restoring a page record their own names.
- [ ] Creating, updating and deleting a user records `user.created|updated|deleted`; the same is true for site, domain, media, plugin, theme and workflow-run
  mutations.
- [ ] Subscribing an endpoint to `page.*` receives a delivery for a newly added `page.*` event without editing the endpoint.
- [ ] `POST /api/v1/webhooks` returns a generated secret exactly once; the subsequent `GET` body contains no `secret` key (asserted in an integration test).
- [ ] `POST /api/v1/webhooks/{id}/secret/rotate` returns a new secret, and a delivery signed with the previous secret fails verification afterwards.
- [ ] The endpoint form rejects: empty name, duplicate name (case-insensitive), URL without a scheme, URL containing whitespace, zero events, more than 32
  events, own secret shorter than 16 chars — each with a field-level message and an API error code.
- [ ] Test delivery reaches a receiver that accepts signed POSTs and leaves a `delivered` row with `attempts >= 1`, `response_status = 200` and a non-null
  `duration_ms`.
- [ ] A receiver answering `500` produces retries with increasing `next_attempt_at`, and the row ends `failed` with `attempts = max_attempts` and a readable
  `error`.
- [ ] `webhook.delivery.failed` is recorded once per exhausted delivery and appears in the feed.
- [ ] Redelivery of a `failed` row resets it to `pending`, increments `redeliver_count`, and a receiver that then answers `200` moves it to `delivered`; a
  second redelivery of the same row beyond the cap is refused with a clear error.
- [ ] Bulk redelivery of 100 ids returns per-id outcomes; pending rows are reported as skipped.
- [ ] Deliveries filter by status, event name and window; the count of returned rows matches the filtered total shown above the table.
- [ ] Disabling an endpoint stops new deliveries but keeps its history readable.
- [ ] `403` is returned (not `404`) when a caller without `webhooks.manage` posts to a management route, and `404` when an endpoint belongs to another
  organization.
- [ ] The event feed's payload inspector copies a JSON path and a copy-as-cURL snippet for a delivery.
- [ ] Retention sweep deletes events outside the window and their deliveries, and is proven by an integration test with a shortened window.
- [ ] The QA walkthrough inventory contains `/webhooks`, `/webhooks/new`, `/webhooks/[id]` and `/events`, all with zero high findings.

### QA plan

Browser walkthrough (per `docs/BUILD-PLAN-v2.md` §4) visits `/webhooks`, creates an endpoint pointed at the development receiver, sends a test delivery, and
follows it into `/webhooks/[id]` → Deliveries; it then switches the receiver to answer `500`, sends a second test, lets it exhaust attempts, and redelivers it
after the receiver is switched back to `200`. It exercises: search, status filter, event-name filter, bulk disable/enable, secret rotation dialog, the
secret-once copy flow, row expansion, copy-as-cURL, `/events` Feed + Catalogue tabs, all filters, the JSON tree, and the mobile drawer at 390 px width. The
visual check must see:
a populated endpoints table with status dots and success-rate numbers (not zeros), a delivery row transitioning `pending → failed → delivered`, a readable error
string on the failed row, an honest empty state before the first endpoint is created, and no dead buttons, disabled menus or "coming soon" labels anywhere on
the screens.

### Slices

1. **Emission coverage + catalogue.** Registry module, emission calls in content/media/identity/ tenancy/plugins/themes/workflows, `GET /api/v1/events/catalogue`,
filter extensions on `/api/v1/events`, `/events` screen with Feed + Catalogue tabs.
*Done line:* publishing a page, uploading media and creating a user each appear in `/events` within one refresh, with correct payloads and working filters.
2. **Endpoint management UI.** `/webhooks` list, create/edit form, secret-once flow, rotation, test delivery, enable/disable, delete — against the existing
endpoints API.
*Done line:* an operator creates an endpoint, receives a signed test delivery at a local receiver, rotates the secret, and sees old signature verification fail.
3. **Delivery operations.** Deliveries tab with filters, row expansion, single + bulk redelivery, stats tab, `webhook.delivery.failed` emission, retention
sweeper.
*Done line:* a deliberately failing receiver produces a `failed` row, redelivery succeeds after the receiver is fixed, and the stats tab shows the changed
success rate.

### Risks / notes

- Delivery throughput depends on the worker running; the QA script must start the engine loop, and a stalled worker must surface as a visible `pending` backlog
  warning on `/webhooks` rather than silence.
- Payloads carry identifiers only — never tokens, secrets, full documents or PII beyond ids; this is asserted in tests, because a public repo makes payload
  shape a compatibility contract.
- Group wildcards are stored expanded; the catalogue-to-endpoint reconciliation runs on subscription and on catalogue growth, and is idempotent.
- Retention deletes history — the default window is documented in the endpoint screen's help text so an operator is never surprised.
- Rate-limit and backoff parameters (`max_attempts` 1–10, backoff base) are configurable per endpoint; defaults are 5 attempts with exponential backoff and
  jitter.
- The current response envelopes are kept for one release; dropping them is a breaking change and belongs to a versioned deprecation, not to this request.
