# REQ-016 — Webhook + Event Bus

> **Status:** in-progress — **slices 1 and 2 are code-complete, API and screens.** **The
> `apps/api/tests/events.rs` suite was dead from tick 59 until 2026-10-01 (`ccefb47b`)** — it
> configured no CSRF secret and minted sessions without signing in, so every
> cookie-authenticated write was refused `403` and ten walks were re-proving that one refusal
> rather than the event bus. Repaired to **11/11**, which is what let the two delivery-measurement
> boxes below close. Slice 1's
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
> **Slice 3 (retention) is now built, and the predicate is the feature.** Migration
> `0123_event_retention.sql` puts the window on the **organization**
> (`organizations.event_retention_days`, `between 1 and 3650`, never null — `null` would mean
> "keep for ever", a decision nobody made deliberately), so changing your own window changes
> what your *next* tick deletes. A `pending` delivery **pins** its event: the obvious sweep
> ("delete old events, let `on delete cascade` take the deliveries") deletes a fact a receiver
> is still owed, and the receiver's only symptom is a delivery that never arrives with nothing
> in the platform saying why. An event nobody was ever queued for is the bulk of the bus,
> which is the part worth deleting. A run that removes nothing is **still logged** — a table
> that only records activity cannot answer "the last sweep was at 03:00 and it found nothing"
> on the day somebody asks why a March event is still in the feed. The worker walks
> organizations oldest-first in bounded batches; `POST /events/retention/sweep` runs one on
> demand. The `/events` **Retention** tab is the third tab beside Feed and Catalogue.
>
> **Proof: `omnion-events --lib` 47** (45 + 2), **`omnion-api --test event_retention` 1/1**
> against real PostgreSQL, **`omnion-api --test events` 9/9** (no regression), **`omnion-api
> --lib` 188**, **`tsc --noEmit` exit 0**. **Still not done, and still not claimed: no browser
> pass** — `runRetentionDepth` and `runWebhooksDepth` are written and **unrun** (the QA slot is
> held by a sibling writer for the whole window), so every acceptance box naming a screen
> stays unticked with the reason in the box.
> **Captured:** 2026-09-25 · **Layer:** core (`crates/webhooks`)
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
- [x] Test delivery reaches a receiver that accepts signed POSTs and leaves a `delivered` row with `attempts >= 1`, `response_status = 200` and a non-null
  `duration_ms`.
      — **closed 2026-10-01, `ccefb47b`**, and the reason it was open for so long is worth the
      space: the whole `events` suite had been **dead** since tick 59 made the session cookie
      ambient authority. It configured no CSRF secret and minted sessions straight through
      `sessions::create_session`, so it held no credential and every cookie-authenticated write
      was refused `403` before the bus saw it — ten walks all re-proving one refusal, and
      reporting **1 passed / 10 failed**. Both refusals were the product working; the fixture was
      the defect. With the fixture repaired (`with_csrf_secret` + a token derived per session and
      packed beside it) the suite is **11/11**, and this box is now measured by
      `a_delivery_row_measures_its_own_duration_and_its_backoff_grows`: a `delivered` row read
      **out of the HTTP body** (the path the screen takes) carries `status = delivered`,
      `attempts = 1`, `response_status = 200`, a non-null non-negative `duration_ms` and no error
      text. `duration_ms` matters because `p95_duration_ms` on the stats tab has no other source,
      and a `—` in that column reads as "instant", which this receiver never reported.
- [x] A receiver answering `500` produces retries with increasing `next_attempt_at`, and the row ends `failed` with `attempts = max_attempts` and a readable
  `error`.
      — Same walk, same commit. The ladder is **climbed one attempt at a time** rather than
      observed across a fixed sleep, because a walk that waits a fixed interval cannot tell
      "the backoff widened" from "the runner happened to tick again later" — only the value in
      the row can. Every reschedule is asserted strictly later than the one before it, and the
      gaps are asserted **exponential**, not merely monotone: a measured run gave 112 ms → 197 ms
      → 358 ms against a 40 ms base, doubling as `retry_delay` promises. The terminal row is
      `failed` with `attempts == max_attempts == 5`, `response_status = 500`, a reason naming the
      500 and **its own duration**, and the receiver is asserted to have seen all five attempts.
      What the walk deliberately does *not* assert is "the schedule was still in the future when
      read back": with a 40 ms base and an HTTP round trip in the tick, that measures the test's
      own latency, and the first version of this assertion failed for exactly that reason while
      the ladder was working perfectly.
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
- [x] Disabling an endpoint stops new deliveries but keeps its history readable.
      — `a_disabled_endpoint_goes_quiet_and_still_answers_what_it_did`: a receiver hears a
      published page, the endpoint is `PATCH`ed to `enabled: false`, a second page publishes and
      is **never queued** (asserted by reading `webhook_deliveries` out of PostgreSQL, not out of
      a response body — queue-then-skip would show the operator a growing list of `pending` rows
      for an endpoint they switched off), and the deliveries made before the switch are still on
      the screen and still `delivered`. The last step switches it back on and publishes a third
      page, so the assertion is that re-enabling *resumes* rather than that disabling is
      destructive: the subscription was muted, never destroyed. A disabled endpoint whose history
      is unreadable is a switch that deletes the answer to "what was this receiver doing last
      Tuesday", and the log is the reason an operator pauses an integration instead of deleting
      it.
- [x] `403` is returned (not `404`) when a caller without `webhooks.manage` posts to a management route, and `404` when an endpoint belongs to another
      organization.
      — The cross-organization half was already proven in `webhooks_are_scoped_per_organization_and_permission_guarded`.
      Slice 2 adds the power boundary the delivery operations introduce: reading an endpoint's
      history is `webhooks.read`, but **sending a delivery again is `webhooks.manage`**, and the
      new walk proves a reader-only account gets `403` from the redelivery route. That split is
      deliberate — a read-only auditor must not be able to make the platform POST to a third
      party by pressing a button.
- [ ] The event feed's payload inspector copies a JSON path and a copy-as-cURL snippet for a delivery.
      — **Both halves are now built; the box stays unticked, for the browser pass.** The
      cURL half was already there and is the reason this box was not closed earlier — it was
      written, and the JSON-path half was not: the inspector's only copy button handed over the
      whole payload, which is a wall of JSON that leaves the reader to find the one key they
      wanted *and* work out how their own receiver spells it. The panel now lists the payload's
      keys as a tree and gives each one the path they are about to type into their code, with
      two details the naive version gets wrong: a key that is not a bare identifier is copied
      as `["order.total"]` rather than `payload.order.total` (which is two lookups and reads
      as a key that does not exist, so the path silently matches nothing in the receiver being
      debugged), and a branch past the depth cap says so with an ellipsis instead of rendering
      as an empty row. The tree is rooted at `payload` because that is the name a receiver
      unmarshals into. The walkthrough step (`4b`) asserts the clipboard *contents* rather than
      the button's presence, because a button copying the key's display name would pass a click
      test and fail the reader. **Unrun, and unticked** — this pass is queued behind a sibling
      that still holds the single QA slot.
- [x] Retention sweep deletes events outside the window and their deliveries, and is proven by an integration test with a shortened window.
      — `the_sweeper_keeps_what_a_receiver_is_still_owed_and_logs_the_rest` against real
      PostgreSQL, on a **one-day** window set through the same `PATCH` an operator uses. It
      proves the four claims the obvious one-statement delete gets wrong: a `pending`
      delivery **pins** its event (aged, queued, swept — both rows still there); a settled
      delivery pins nothing (the same sweep removes both); the window is **per
      organization** (a two-day tenant's sweep does not touch a thirty-day-old row belonging
      to another); and a sweep that removes nothing is **still logged**, because "the last
      sweep was at 03:00 and it found nothing" is the sentence an operator needs on the day
      they ask why a March event is still in the feed. Every count is read back out of the
      database, never from the response body — a response that omits a field is
      indistinguishable from one that stored it and chose not to say so.
      The window is a column on `organizations` (migration `0123`, `between 1 and 3650`, never
      null, default 30) rather than a column on the event, so an organization that changes
      its own window changes what its *next* tick deletes; the background worker
      (`OMNION_EVENT_RETENTION_RUNNER`, on by default) walks organizations oldest-first in
      bounded batches, and `POST /events/retention/sweep` runs one on demand for both
      `webhooks.manage` holders and the `/events` Retention tab.
- [ ] The QA walkthrough inventory contains `/webhooks`, `/webhooks/new`, `/webhooks/[id]` and `/events`, all with zero high findings.
      — `/webhooks` and `/webhooks/new` are now in the routes list, `/events?tab=retention`
      joins them, and `runWebhooksDepth` + `runRetentionDepth` are written (`runRetentionDepth`
      checks the tab, the server-supplied bounds, that a value outside the range disables
      Save rather than offering a `400`, that the save is audited with **both** window values,
      that a sweep answers with a sentence including "found nothing", that the run log grew,
      and it restores the window in a `finally` so a mid-run throw cannot leave the QA
      organization's bus narrowed for every pass after it). `/webhooks/[id]` is deliberately
      not in the list: a route walked with a dummy id proves only that the not-found state
      renders, and `runWebhooksDepth` opens a *real* endpoint instead.
      **Unrun, and unticked** — the QA slot is held by a sibling writer for the whole window
      and the box will be closed on the pass's result, not before.

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
