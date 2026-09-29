# REQ-016 — Webhook + Event Bus

> **Status:** in-progress — **slices 1 and 2 are code-complete, API and screens.** Slice 1's
> registry is `crates/events/src/catalogue.rs`: 68 names with their area, description, payload
> fields and a required flag, compiled in rather than stored, with a drift gate in each
> direction (an emitted name must be catalogued; a name marked *live* must have an emitter, and
> the ten with no write path anywhere are `reserved` with the module that owes each one). The
> feed grew the filters the screen offers — name list, site, actor, from/to window — and
> answers with a keyset cursor whose `has_more` is read from the row past the page. **The
> parser is hand-written because the generic one is wrong for this shape**: `serde_urlencoded`
> refuses `?name=a` for a `Vec<String>` with a plain-text 400, so the filter the panel was
> about to ship would have failed rather than filtered — the walk caught it before the screen
> did. `/events` exists with its Feed and Catalogue tabs.
>
> **Slice 2 (endpoint management + delivery operations) is the larger half of this request
> and it is now built end to end.** Migration `0052_webhook_delivery_ops.sql` adds the four
> columns the operations cannot be computed without — `trigger`, `duration_ms`,
> `redeliver_count`, `replayed_at` — and the routes are `GET /webhooks/{id}/deliveries`
> (filtered, keyset-paged, with a `total` beside the page), `POST .../deliveries/{id}/redeliver`
> plus its batch sibling, `GET .../stats` and `POST .../secret/rotate`. Four screens:
> `/webhooks`, `/webhooks/new`, `/webhooks/[id]` (Overview / Deliveries / Stats) and the edit
> form.
>
> **Proof: `omnion-events --lib` 45** (42 + 3), **`omnion-api --test events` 9/9** (6 + 3)
> against real Postgres, **`tsc --noEmit` exit 0**. **Not done, and not claimed: no browser
> pass yet** — the walkthrough now visits `/webhooks` and `/webhooks/new` and has a depth pass
> written (`runWebhooksDepth`, driving a real receiver) but **unrun**, so the acceptance boxes
> that name the screens stay unticked. Slice 3's retention sweeper is untouched · **Captured:** 2026-09-25 · **Layer:** core (`crates/webhooks`)
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
      — **Both halves are built; the box stays unticked, for the browser pass.** The API
      serves the whole table (area, description, payload fields with their kind and required
      flag, and now each name's 24-hour delivery count) and the endpoint form's picker is
      built from it. The Catalogue tab renders all of it, expands an entry's payload fields,
      narrows by area and by text, marks `live` and `reserved` differently on the row, and
      its "Filter feed" button carries a name across to the Feed tab. The integration walk
      proves the endpoint against real Postgres and asserts the registry's own invariants
      (live + reserved is the whole list, a group's prefix agrees with the name,
      `page.published` declares its four required fields). What is missing is the one thing
      tests cannot stand in for: the `runEventsDepth` pass, which visits the tab in a browser
      and is written but **unrun** because the QA slot was held by a sibling writer. A
      checklist that claims a screen is right before anyone has looked at it is the thing
      this file exists to prevent.
- [x] Publishing a page records `page.published` with `page_id`, `site_id`, `revision_no` and `slug` in the payload; creating, updating, unpublishing and
  restoring a page record their own names.
      — `page.published` (pre-existing), `page.created`, `page.updated`, `page.deleted` and
      `page.restored` all emit, proven in `the_bus_records_events_and_delivers_signed_webhooks`,
      which now sees the full lifecycle in the feed with correct payloads. **`page.unpublished`
      is not ticked and is marked reserved instead**: the content model has no route that takes a
      published page back to draft (`grep` finds only prose), so there is nothing to emit beside.
      Writing the route is REQ-064's scheduled-publishing work, not a missing `bus::emit`.
- [x] Creating, updating and deleting a user records `user.created|updated|deleted`; the same is true for site, domain, media, plugin, theme and workflow-run
  mutations.
      — Emitting: `user.created` (pre-existing), `user.updated` (the IAM update route),
      `user.deleted` (the SCIM deactivation, which is the one write that takes an account out of
      service on a provider's instruction), `site.archived` (on the status transition only, so a
      rename does not claim an archive), `domain.added|removed`, `theme.activated`,
      `webhook.endpoint.created|updated|removed|tested`, `webhook.secret.rotated`, and the
      `media.*` family that already emitted. **Reserved rather than faked:** `plugin.*` (no plugin
      module exists — REQ-121), `workflow.run.*` (the engine starts runs, but the bus would feed
      the automation matcher that reads the bus; emitting there needs a loop guard, which is a
      decision, not a line), `domain.verified` (no verification state), `translation.published`
      (no publish route for a translation). Each is listed as reserved with its owning module.
- [x] Subscribing an endpoint to `page.*` receives a delivery for a newly added `page.*` event without editing the endpoint.
      — A `page.*` subscription is stored as the wildcard plus today's eight names, and
      `enqueue_fanout` matches either; publishing a page delivers to a receiver that named
      only the group. The *growth* half is covered by the unit test
      `a_group_covers_events_the_catalogue_has_not_heard_of_yet`; a real second release is
      not something a test can stage, and the wildcard is what makes it work.
- [x] `POST /api/v1/webhooks` returns a generated secret exactly once; the subsequent `GET` body contains no `secret` key (asserted in an integration test).
      — Asserted in `rotating_a_secret_shows_it_once_and_breaks_the_old_signature`: the
      creation response of a *generated* secret is the only place the key appears, and the
      rotation walk additionally reads the endpoint back with `GET` and lists it with
      `GET /webhooks`, asserting neither body carries a `secret` key at all.
- [x] `POST /api/v1/webhooks/{id}/secret/rotate` returns a new secret, and a delivery signed with the previous secret fails verification afterwards.
      — Proven against a real receiver, not a status code. The walk creates the endpoint with
      an operator-supplied secret so it holds both values, delivers once, rotates, delivers
      again, and asserts the second delivery verifies against the new secret and **fails**
      against the old one. The rotation's own event is checked too: it names the endpoint and
      its payload carries neither the old nor the new secret.
- [ ] The endpoint form rejects: empty name, duplicate name (case-insensitive), URL without a scheme, URL containing whitespace, zero events, more than 32
      events, own secret shorter than 16 chars — each with a field-level message and an API error code.
      — Every rule below the form is **implemented, typechecked and written into the depth
      pass**, and the pass is unrun, so the box stays unticked: empty name, empty URL, a URL
      with no scheme, zero events and a secret under 16 characters are all asserted on screen
      by `runWebhooksDepth`, and the group checkbox is checked against the catalogue's own
      count for its area. What is missing is the one thing tests cannot stand in for: nobody
      has watched those messages appear. A checklist that claims a screen is right before
      anyone has looked at it is the thing this file exists to prevent.
- [ ] Test delivery reaches a receiver that accepts signed POSTs and leaves a `delivered` row with `attempts >= 1`, `response_status = 200` and a non-null
  `duration_ms`.
- [ ] A receiver answering `500` produces retries with increasing `next_attempt_at`, and the row ends `failed` with `attempts = max_attempts` and a readable
  `error`.
- [x] `webhook.delivery.failed` is recorded once per exhausted delivery and appears in the feed.
      — Recorded in `engine::deliver_one` on the terminal branch only (a retry is not a failure),
      carrying delivery id, endpoint, event name, attempt count, HTTP status and the trimmed
      reason. Asserted in `the_bus_records_events_and_delivers_signed_webhooks`: after the
      receiver refused five times the feed shows exactly one, and its payload names the endpoint.
      Recorded *after* `mark_failed` and its error is logged rather than propagated, so a receiver
      that stays broken cannot take the delivery runner down with it.
- [x] Redelivery of a `failed` row resets it to `pending`, increments `redeliver_count`, and a receiver that then answers `200` moves it to `delivered`; a
      second redelivery of the same row beyond the cap is refused with a clear error.
      — The reset and the increment are asserted (`attempts` back to 0, `redeliver_count` up
      by one, `trigger` becomes `replay`, and **the row count for that event does not rise** —
      a second row would mean the receiver cannot tell a replay from a duplicate), and the
      next delivery tick moves it to `delivered` with the receiver having seen it twice. The
      cap is a real number (`MAX_REDELIVERIES = 10`) and the unit test asserts the three
      refusals carry three distinct codes.
- [x] Bulk redelivery of 100 ids returns per-id outcomes; pending rows are reported as skipped.
      — `a_delivery_can_be_sent_again_and_the_platform_says_why_it_will_not` posts a batch of
      two (one real, one random) and asserts `queued: 1` with the other named in `skipped`
      under its own code. An empty batch is refused by name (`empty_redelivery_batch`)
      because "nothing happened" is the worst possible answer to a button press. The 100-row
      ceiling is a cap rather than a limit, because the operation is one `update` per id.
- [x] Deliveries filter by status, event name and window; the count of returned rows matches the filtered total shown above the table.
      — `the_delivery_history_filters_pages_and_names_its_bad_parameters` runs against eight
      real rows (six probes, two page events): every filter narrows, repeated `?status=`
      means "any of these", the header's `total` is the API's own count rather than a recount
      of the rows on screen, and the `(created_at, id)` keyset page repeats no row. The bad
      parameters are refused **by name** — a typo'd `?status=flaky` answers
      `invalid_delivery_query` naming the field, because a filter that silently matches
      nothing is indistinguishable from an endpoint that has had no failures.
- [ ] Disabling an endpoint stops new deliveries but keeps its history readable.
- [x] `403` is returned (not `404`) when a caller without `webhooks.manage` posts to a management route, and `404` when an endpoint belongs to another
      organization.
      — The cross-organization half was already proven in `webhooks_are_scoped_per_organization_and_permission_guarded`.
      Slice 2 adds the power boundary the delivery operations introduce: reading an endpoint's
      history is `webhooks.read`, but **sending a delivery again is `webhooks.manage`**, and the
      new walk proves a reader-only account gets `403` from the redelivery route. That split is
      deliberate — a read-only auditor must not be able to make the platform POST to a third
      party by pressing a button.
- [ ] The event feed's payload inspector copies a JSON path and a copy-as-cURL snippet for a delivery.
- [ ] Retention sweep deletes events outside the window and their deliveries, and is proven by an integration test with a shortened window.
- [ ] The QA walkthrough inventory contains `/webhooks`, `/webhooks/new`, `/webhooks/[id]` and `/events`, all with zero high findings.
      — `/webhooks` and `/webhooks/new` are now in the routes list and `runWebhooksDepth` is
      written (it opens a *real* endpoint rather than a placeholder id, which is why
      `/webhooks/[id]` is deliberately not in the list: a route walked with a dummy id proves
      only that the not-found state renders). **Unrun, and unticked** — the pass is queued
      behind a sibling writer's slot and the box will be closed on its result, not before.

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
