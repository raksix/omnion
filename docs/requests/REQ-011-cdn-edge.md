# REQ-011 — CDN / Edge System

> **Status:** in-progress (slices 1–3 built and green — `807e307`, `69ba2c7`, `3df6432`; **slice 4's credential path shipped this tick.** It was the one control on `/cdn/settings` that had been a button reporting success and storing nothing: the panel has rendered a `Replace credential` field since slice 1, sends it, and the handler dropped the value on the floor with a comment saying it would — so `has_credential` was pinned to `false` for ever and every adapter that needs a key could never authenticate. The column existed, the field existed, the `needs_credential` flag existed, and nothing connected them. Now: `crates/cdn/src/credential.rs` seals it with the platform's own envelope (`omnion_identity::SecretBox`, one key variable, one development fallback) and stamps a `omnion-cdn-credential.v1:` prefix so a value written by another build reads as a corrupt row rather than as a key; the `PUT` seals before it writes, so a refused credential never leaves the rest of the save half-applied; and `provider_for_site` — the *worker's* read, which is what makes the two halves one contract — decrypts instead of hardcoding `None`. An unreadable envelope is `CredentialUnreadable`, a `500`: the operator did nothing wrong, and answering `400 invalid_credential` would send them to paste the key they just pasted. The suite also had **no CSRF token at all** — `login()` read one `Set-Cookie` and kept the session — so every mutation in `cdn_purge.rs` was refused `csrf_unavailable` before its handler and the walks were measuring a 403. Same `Credentials { session, csrf }` + `Deref` type as tick 80, plus the `config.csrf = CsrfSecret::new(..)` line the media suite already carries and documents. `origin/main` merged in `9cc17473` · **Captured:** 2026-09-25 · **Layer:** platform / infra
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

```text
User
 ↓
CDN
 ↓
Edge Cache
 ↓
Omnion
```

Cache invalidation:

```text
Page published
     ↓
Purge CDN cache
     ↓
New version live
```

## Implementation spec

New crate `crates/cdn` (cache rules + purge queue + provider adapters), an admin section `/cdn`, and cache headers on the public surface of `apps/api`. It stands on `crates/storage`, `crates/events` and `crates/audit`; nothing else in the platform talks to a CDN directly.

### Scope (in / out)

**In**

- `crates/cdn`: cache-rule model and matcher, cache-key derivation, provider adapter trait (`purge_urls`, `purge_tags`, `purge_all`, `verify`), purge queue with bounded retries, per-request cache status counters, signed URL helper for private media.
- Adapters shipping in this request: `origin` (no external cache — rule engine and headers only), `generic_http` (POST a JSON purge payload to a configured endpoint), `cloudflare_style` (zone purge calls matching a hosted-CDN zone API). The catalogue endpoint lists only shipped adapters.
- Cache policy output on the public API surface: `Cache-Control`, `CDN-Cache-Control`, `ETag`, `Vary`, `Surrogate-Key` on `/api/v1/public/*` and `/api/v1/media/*`; immutable caching for content-hashed theme assets.
- Admin: CDN overview, purge console, cache-rule editor, provider settings, purge history.
- Automatic purge triggers: `page.published`, `page.unpublished`, `page.deleted`, `media.version_created`, `theme.activated`, `domain.added`, `domain.removed` — each mapped to URLs and tags by the rule set.
- A purge worker (drain loop, exponential backoff, batch cap, per-provider concurrency limit).

**Out**

- Running our own edge fleet or proxying traffic ourselves; DNS and zone management; WAF/bot rules; on-the-fly image transformation; edge compute; multi-region topology (REQ-035); request-level analytics beyond the counters stored here; cache warming.

### Screens (UI)

- `/cdn` — overview: status cards (active provider, purge queue depth, purges 24h, failure rate), "Purge" primary action, "Run provider check" action, recent-purge table, empty state when no provider is configured yet.
- `/cdn/purges` — purge history table: **When · Kind (url/tag/all) · Target summary · Site · Item count · Failed · Status · Requested by**. Filters: status (`queued|running|partial|failed| succeeded`), kind, site, date range. Row opens a detail drawer with per-item rows (target, attempts, response status, error). Bulk actions: retry failed, copy targets, export CSV.
- `/cdn/purge` — purge console form: mode radio (URLs / tags / everything), target textarea (one URL or tag per line, max 500 entries, each validated as an absolute path or tag pattern), site select, "also purge the provider's whole zone" switch (requires the typed confirmation `PURGE`), submit. Inline validation: empty target list, malformed URL, more than the cap, unknown tag format.
- `/cdn/rules` — cache-rule table: **Priority · Name · Path pattern · Methods · Edge TTL · Browser TTL · SWR · Status**. Drag handle reorders priority (persisted through the reorder endpoint). Filters: enabled/disabled, method, "bypass only". Row actions: edit, duplicate, disable, delete (confirm). Bulk: enable, disable, delete.
- `/cdn/rules/new` and `/cdn/rules/{id}` — rule form fields: name (1–64, required, unique per site), path pattern (required; `*`/`**` glob with a live match tester against a sample URL), methods multi-select (default GET/HEAD), edge TTL seconds (0–31536000), browser TTL seconds, stale-while- revalidate seconds, cache-key components (host, path, query allow-list, language cookie), bypass conditions (cookie names, query params, header names), enabled switch. Validation messages appear under the field; save is blocked until clean.
- `/cdn/settings` — provider card (adapter pick, endpoint URL, zone reference), credential fields are write-only (`Replace credential` affordance, never rendered back), automatic-purge toggles per trigger event, batch size (1–1000), max attempts (1–10), "Test connection" with an inline result line (latency, HTTP status, message).
- States: skeleton rows while loading; per-screen empty states with a single primary action; error banner with a retry that re-runs the failing request; a purge whose provider call fails shows `failed` with the provider message, never a silent drop.
- Keyboard: `p` opens the purge console, `n` new rule, `/` focuses filter, `⌘K` opens the command palette, `Esc` closes drawers. Mobile: tables become stacked cards showing the four key columns; the purge console and rule form are single-column, full width.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/cdn/status` | Provider state, queue depth, 24h counters | `cdn.read` |
| GET | `/api/v1/cdn/settings` | Per-site CDN settings (credentials masked) | `cdn.read` |
| PUT | `/api/v1/cdn/settings` | Save provider, triggers, batch and retry settings | `cdn.manage` |
| POST | `/api/v1/cdn/settings/test` | Run a provider reachability check | `cdn.manage` |
| GET | `/api/v1/cdn/adapters` | Shipped adapter catalogue + config schema | `cdn.read` |
| GET | `/api/v1/cdn/rules` | List cache rules in priority order | `cdn.read` |
| POST | `/api/v1/cdn/rules` | Create a cache rule | `cdn.manage` |
| GET | `/api/v1/cdn/rules/{id}` | Read one rule | `cdn.read` |
| PUT | `/api/v1/cdn/rules/{id}` | Update a rule | `cdn.manage` |
| DELETE | `/api/v1/cdn/rules/{id}` | Delete a rule | `cdn.manage` |
| POST | `/api/v1/cdn/rules/reorder` | Persist rule priority order | `cdn.manage` |
| GET | `/api/v1/cdn/purges` | Purge history (paged, filterable) | `cdn.read` |
| POST | `/api/v1/cdn/purges` | Request a purge (urls / tags / all) | `cdn.purge` |
| GET | `/api/v1/cdn/purges/{id}` | Purge detail with per-item results | `cdn.read` |
| POST | `/api/v1/cdn/purges/{id}/retry` | Requeue failed items | `cdn.manage` |

Public read endpoints that emit cache headers (`/api/v1/public/pages/{slug}`, `/api/v1/public/media/{id}`) carry no permission, as today. New catalogue keys: `cdn.read`, `cdn.manage`, `cdn.purge` (category `cdn`).

### Data model

**`cdn_settings`** — one row per site; `site_id` null means the platform default. `id uuid pk default gen_random_uuid()`, `site_id uuid null → sites(id) on delete cascade`, `provider text not null default 'origin'` (in `origin|generic_http|cloudflare_style`), `endpoint_url text null`, `zone_ref text null`, `credential_ciphertext bytea null` (write-only; never returned by the API), `auto_purge jsonb not null default '{}'` (trigger → bool), `batch_size int not null default 100`, `max_attempts int not null default 5`, `updated_by uuid → users(id)`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`. Constraints: `batch_size between 1 and 1000`, `max_attempts between 1 and 10`, unique `(site_id)` with a separate partial unique index for the platform row.

**`cdn_cache_rules`** — `id uuid pk default gen_random_uuid()`, `site_id uuid not null → sites(id) on delete cascade`, `name text not null` (1–64), `priority int not null`, `path_pattern text not null`, `methods text[] not null default '{GET,HEAD}'`, `edge_ttl_seconds int not null default 300`, `browser_ttl_seconds int not null default 60`, `swr_seconds int not null default 0`, `cache_key jsonb not null default '{}'`, `bypass jsonb not null default '{}'`, `enabled boolean not null default true`, `created_by uuid → users(id)`, `created_at`, `updated_at`. Constraints: `edge_ttl_seconds between 0 and 31536000`, `browser_ttl_seconds between 0 and 31536000`; unique index `on (site_id, lower(name))`; index `(site_id, priority)`.

**`cdn_purges`** — `id uuid pk default gen_random_uuid()`, `site_id uuid → sites(id) on delete set null`, `kind text not null` (`url|tag|all`), `targets text[] not null default '{}'`, `status text not null default 'queued'` (`queued|running|succeeded|partial|failed`), `provider text not null`, `item_count int not null default 0`, `failed_count int not null default 0`, `requested_by uuid → users(id) on delete set null`, `requested_at timestamptz not null default now()`, `started_at timestamptz`, `finished_at timestamptz`, `error text`.

**`cdn_purge_items`** — `id bigint generated always as identity pk`, `purge_id uuid not null → cdn_purges(id) on delete cascade`, `target text not null`, `status text not null default 'pending'`, `attempts int not null default 0`, `next_attempt_at timestamptz not null default now()`, `response_status int`, `error text`, `done_at timestamptz`.

Indexes: `cdn_purges (site_id, requested_at desc)`; `cdn_purge_items (purge_id, status)`; `cdn_purge_items (next_attempt_at) where status = 'pending'`. Migration: `database/migrations/0011_cdn_edge.sql` (append-only, commented in the style of `0009`).

### Events

**Emitted:** `cdn.purge.requested`, `cdn.purge.completed`, `cdn.purge.failed`, `cdn.rule.changed`, `cdn.settings.updated` — each with the identifiers a consumer needs (purge id, site id, kind, counts) and no rendered content. **Consumed:** `page.published`, `page.unpublished`, `page.deleted`, `media.version_created`, `theme.activated`, `domain.added`, `domain.removed`; a consumed event maps to URLs and tags through the rule set, then enqueues one purge.

Webhook relevance: `cdn.purge.failed` is subscribable so an operations endpoint can alert on cache drift; `cdn.purge.completed` is deliberately low-volume and also subscribable. Audit entries use the `cdn.*` namespace (`cdn.purge.requested`, `cdn.settings.updated`, `cdn.rule.changed`).

### Acceptance criteria

- [x] `crates/cdn` exists with a provider adapter trait and the three shipped adapters, unit-tested. (`ad1eae0`, `cba486c`)
- [x] Migration applies on a fresh database and on one with existing rows. (shipped as `0048_cdn_edge.sql` — `0011` was taken; also fixed the `unique (coalesce(...))` in `0047_media_grants.sql`, which is what actually stopped every migration after it from applying. `d11c9e9`, `262bb98`)
- [x] `/cdn` shows real provider state, queue depth and the last 20 purges from the API.
  _The provider card, the rule counts and the unreadable-rule count are read from the API
  (`/cdn/settings` + `/cdn/rules`); the overview ships at `/cdn`. The purge-queue depth and
  the purge table are slice 2's, and the overview says so rather than rendering a `0` for a
  counter nothing has ever written — a fabricated zero makes the first real zero
  indistinguishable from it, and the two states need different fixes._
- [x] Create, edit, disable, delete and reorder cache rules from `/cdn/rules`; priority persists.
  _API half proved `0e2993c` (15/15); the screens shipped this tick (`ee421ad`, `807e307`,
  `8ca76f0`) and the browser pass drives the whole row: create, edit, disable, enable,
  duplicate, delete-with-confirmation and an up/down reorder. The depth pass reads the
  resulting order **back from the API** rather than from the table, and asserts the
  priorities are a dense ascending run — the property a per-row renumber breaks, and the
  one that leaves two rules claiming the same priority and a tie the matcher breaks by row
  order rather than by what the drag showed._
- [x] The rule form rejects an empty name, a malformed pattern and a TTL above the cap with a field message. (API half: each refusal names its own field. `0e2993c`)
- [x] Purge by URL list runs end to end and the history row reaches `succeeded` with per-item results.
  _`a_purge_by_url_list_is_written_and_answers_with_its_own_state` and
  `a_successful_drain_leaves_the_purge_succeeded_with_every_item_done`. The second is the
  one that matters: it calls the worker's own `claim_due` / `apply_outcome` / `settle` — the
  same functions the binary calls, not a copy of them, so the status the panel renders is a
  status something actually computed. A test that stopped at "the API accepted it" would
  prove only that a row was written. Suite: **17/17**._
- [x] Purge by tag and "everything" both work; "everything" requires the typed confirmation.
  _Both kinds are accepted in
  `a_purge_by_tag_and_a_whole_zone_purge_are_both_accepted`, and the refusal is proved
  separately: `a_whole_zone_purge_without_the_typed_word_is_refused` asserts the 400 **and**
  that the history is still empty afterwards. That second assertion is the one worth having —
  a rejected zone purge that still left a queued row would be discovered an hour later by
  somebody who had been told it did not happen._
- [x] Publishing a page produces a purge row automatically within one worker tick, honouring trigger toggles.
  _`crates/cdn/src/invalidation.rs` is the automatic half: a trigger table, a **pure** `plan`
  from `(event, payload, toggles, provider capabilities)` to the purge that would be queued, and
  a durable-cursor walk over `events` that writes the plans. The pure half is deliberate — the
  mapping is where every real choice lives, and a mapping testable only by emitting an event and
  reading a table is a mapping nobody writes a second test for. Sixteen such tests cost 0.02s;
  nine integration walks against the real bus — 10 of them, 3.15s — are the ones that would catch a
 statement PostgreSQL refuses. The drain runs **before** the drain that claims due items, so a
  publication is queued and claimed in the same worker tick rather than the next one.
  Trigger toggles are honoured per name and default to **off**: a settings row created before a
  trigger existed must not start purging a site because someone added an event name, and the
  operator has a switch for every name precisely so the answer is theirs to give.
  Proved by `apps/api/tests/cdn_invalidation.rs` — **10/10** — the publish walk (a purge row,
  one item, the page's own address, `requested_by = null`, and a `cdn_purge_sources` row naming
  the event), the exactly-once walk (drain three times → one purge), the disabled-toggle walk,
  the no-address walk, the foreign-event walk, the media-address walk, the theme walk, the
  tag-fallback walk, and one that carries the queued row to `succeeded` through the worker's own
  `claim_due` / `apply_outcome` / `settle`.

  Two of those walks exist because they were **wrong first**, and the corrections are the
  argument. Every shipped adapter reports tag support — `origin` because a tag resolves to its
  URL when there is no edge at all — so the planner's tag-less branch is unreachable in
  production today; rather than fake a tag-less provider and bank a green walk for the wrong
  reason, that branch is driven through the planner and the walk names the condition under which
  it should be promoted. And a trigger turned on after the fact does **not** retroactively purge
  what it missed: the cursor has passed, and replaying a month of backlog is a stampede at a
  provider for content that has been republished many times since. The console is the tool for
  "purge everything now", and it says so._
- [x] A provider error marks the purge `failed`, records the provider message and leaves items retryable.
  _`a_provider_refusal_lands_in_the_drawer_with_its_message_and_is_retryable` points the site
  at an adapter with an unreachable endpoint and drains it three times against a budget of
  two attempts. The walk asserts the *intermediate* state too — after one drain the items
  are `pending` with `attempts = 1`, not `failed` — because "fails and is marked failed on
  the first refusal" and "fails and is retried" are different products, and only the second
  one survives a provider that was briefly down. The message is checked on the item row,
  the parent row, and the drawer._
- [x] Retry from the history drawer requeues only failed items and updates the counts.
  _`a_partial_drain_says_partial_and_counts_only_what_failed` drains one purge into a
  `done`/`failed` mix, then retries and asserts the resulting statuses are exactly
  `["done", "pending"]`. Re-running the whole purge would re-send targets the provider
  already accepted, which on a metered provider is slower *and* adds rate-limit pressure to
  an outage. The retryable assertion is server-side: the walk
  `a_retry_on_a_purge_with_nothing_to_retry_is_refused_with_an_explanation` checks that a
  `queued` purge answers 409 and names its own state, rather than accepting a request that
  would do nothing._
- [x] Public page and media responses carry `Cache-Control`, `ETag` and `Surrogate-Key` headers. (`ca65085`, 13/13)
- [x] Media responses answer `If-None-Match` with `304` and a matching `ETag`. (`69918e2`, `29b3409`)
  _The handler was written a while before anything proved it, and the page walk could not
  substitute: a media file is a different route with a different validator (the checksum, not
  the revision number), addressed by an id rather than a slug, and matched against
  `/api/v1/public/media/{id}` — a path nobody types, which is exactly why a rule for it is
  easy to write wrong and worth proving. `a_media_file_answers_a_conditional_read_with_304_and_the_same_validator`
  uploads through the real route (a row whose bytes are not in storage is a 500 from the
  storage layer, and a test asserting on a 500 is not a test of the cache), marks it scanned
  and then reads it twice. Two more walks came with it, and both found something:
  `a_cache_rule_written_for_the_media_path_changes_what_a_visitor_keeps` asserts the two TTLs
  land in two *different* headers — `Cache-Control: max-age` is the browser's and
  `CDN-Cache-Control` is the edge's, which is the easy thing to read wrongly, and a rule
  that sets an hour at the edge and a minute in the browser is the ordinary configuration;
  and `a_file_the_scanner_has_not_cleared_is_refused_before_any_cache_header` asserts the
  *order* of the two checks, because a refusal carrying a validator is a refusal an
  intermediary is entitled to remember, and a scan that finishes an hour later leaves the
  cached 403 sitting in front of it. Suite: **16/16**._
- [x] Purge console rejects more than 500 targets and an invalid URL with a clear message.
  _`more_than_the_cap_is_refused_and_the_message_carries_the_count` tries 501 and 500. The
  second half is the part worth having: a test that only tried the refusal would not notice
  an off-by-one that locked out a legitimate maximum, and the cap is inclusive. The malformed
  cases (relative path, `//` prefix, embedded whitespace, a bad tag, an empty list, an
  unknown kind) are in one walk, each asserting its **own** code — six refusals sharing one
  "invalid" message is a form nobody can act on._
- [x] Every mutation writes an audit entry under the `cdn.*` namespace with actor and IP. (`0e2993c`)
- [x] All endpoints are guarded by the catalogue keys and a forbidden call returns `403 permission_denied`. (`0e2993c`)
- [x] Filters, empty, loading and error states exist on every screen; the rows shown match the API counts.
  _**Tick 86: the scoped pass finally ran, and every clause of it was reading the harness's own
  arithmetic.** Six passes across three commits, and the first one walked nothing at all: `wants()`
  tested scope names for **equality** while a scope is written the way a person thinks about a
  screen (`cdn`) and the names it has to match are spelled three ways — `cdn-overview`/`cdn-rules`
  in the route list, `cdnRules`/`cdnPurges` as depth-pass names. So `--only=cdn` reported
  `0 route/pass name(s) walked, 1 unmatched`, skipped every CDN depth pass as "out of scope",
  and exited **0** with four high findings. The unmatched guard is the only reason that was
  visible rather than a green pass measuring nothing, so it earned its keep on the first use.

  The next three defects are one cause: `/cdn/rules` and `/cdn/purges` each render a table from
  `md` up and cards below it, and **both renderings carry the same `data-cdn-*` hooks** —
  deliberately, since a hook in only one rendering halves what a depth pass can drive. So a bare
  `.count()` reads both halves. The consequences were three claims that looked like product
  bugs and were not:

  | Claim | Said | Was |
  | --- | --- | --- |
  | `countMatches` (purges) | rows 4 vs header "Showing 2 of 2" | 2 rows × table + card |
  | `cdnRules.ok` | reorder broken, `steps: 0` | strict-mode violation: the `data-cdn-rule-up` locator resolved to **2 elements**, so the click died before the panel was asked to move anything |
  | `filterNarrows` / `clearedBackToRows` | the status filter does not narrow | `4 < 2` is false for a filter that works |

  Every count is now `:visible`, and every driven control names the layout it means. `cleanedUp`
  was the subtle one: the viewport had been restored to 1280, so it counted the table *and* the
  still-mounted card list while `before` counted one rendering. `mobileRows` and the
  touch-target `querySelectorAll` stay bare **on purpose** — they run at 390px where only the
  cards exist, and there a bare count is the measurement of the mobile layout, not a shortcut.

  The scope rollup had the mirror defect: it asked `matchedOnly.has(name)` — exact — *after*
  `wants()` became a prefix test, so it fired `unknown-pass-name` on a scope the pass had just
  walked. **A guard that fires on a scope it honoured is worse than no guard**: it teaches the
  reader to ignore the one line that catches a pass pointed at nothing.

  What the pass reports with the counts fixed: `reorderSwapped: true`, `toggled: true`,
  `duplicated: true`, `ttlRefusalNamesTheBound: true`, `errorState: true`,
  `emptyStateIsNotTheUnfilteredOne: true`, `escClosedDrawer: true`, `mobileTableHidden: true`,
  and `mobileTouchTargets: true` over 15 controls with the smallest at 35.3px against the 32px
  floor. The touch-target measurement was built last tick to *name* the short control; this pass
  is the first to report through it, and the answer is that nothing is short._
  _**For the record, the two blockers that preceded this.** The measurement was built at
  `04bc7e73` and had not reported for 55 ticks, and the reason recorded throughout that time was
  a harness-flakiness story — the tick-21 pass reporting "0 elements" on `/cdn/purges`, which sent
  every tick looking at the pass instead of at the pass's own arithmetic. The real finding was
  that the count match **had no instrument at all**: `runCdnPurgeDepth` stored the header sentence and
  `runCdnRulesDepth` stored a row count, and neither compared either to the API, so the clause
  could not have been ticked by any pass. `04bc7e73` builds it — two deterministic hooks, a
  three-way comparison of DOM rows / API `total` / rendered sentence that **refuses to assert on a
  single row** (where `Showing N of N` is true by construction), a deliberately different assertion
  for the unpaged rules screen whose failure mode is a dropped row rather than a hidden page, and a
  drive of the filtered empty state that no pass had reached (`status=failed AND kind=tag` empties
  the table while the unfiltered one has rows — the only way to tell "nothing matches this filter"
  from "no purges yet")._
  **Tick 82: the filter this clause measures was a control that could not express its own
  values, so the clause was not merely unmeasured — it was unmeetable.** The purge history's
  status filter was an `<input type="search">` whose raw string went to `?status=`.
  `PurgeStatus::parse` returns `None` for anything it does not recognise and the route
  deliberately turns that into *no filter* rather than a 400, which is the right call for an
  API and a hazard for a control a person types into: `fail` filters nothing, `Failed` filters
  nothing because the stored spelling is lowercase, and the screen has no way to say which of
  those two happened. The `STATUSES` table with all five labels had been in the file since
  slice 1, written for this control and rendered by nothing. It is a `<select>` now
  (`60ff5567`), `/` still focuses it so the keyboard line stays honest, and the sweep for the
  class across the whole pass found 30 `selectOption` call sites and no other instance.

  The second half of that fix is the part worth keeping. `runCdnPurgeDepth` drove the control
  with `selectOption("failed")` inside a `.catch(() => {})`: on an `<input>` the call throws,
  the catch ate it, and the pass recorded `filterNarrows: false` — a sentence the summary
  reads as *the product's filter does not narrow its rows*. "The harness could not drive the
  control" and "the control does not work" are the same line, and only the second is
  actionable. There is no catch on that interaction now and the control's tag name is recorded
  beside the outcome, so the two readings can never collapse into one number again.

  **Tick 82: the first scoped pass over these screens ran, and it found a cross-tenant write.**
  The pass reported (verbatim, from `qa-artifacts`, which is what the numbers below come from):
  the count match `true` on both screens, the live tester answering both ways, the TTL refusal
  naming its bound, `createdInTable: true`, `duplicated: true`, `errorState: true`, and
  `mobileNoTableScroll: true` — against two reds: `reorderSwapped: false` and
  `toggled: false`.

  **`toggled: false` was a real bug, and a serious one.**
  `POST /api/v1/cdn/rules/{id}/toggle` took a `site_id` from the request body, used only the
  path id in its `WHERE` clause, and then compared the row it had **already written** against
  the caller's site to answer `403 permission_denied`. So a caller in organization A naming a
  rule id belonging to B got a refusal and changed B's cache rules anyway. The site's
  predicate is in the statement now, and the check is gone rather than moved: a foreign id
  matches no row, which is the same `404` a nonexistent id gives, instead of the old shape's
  `403`-for-foreign / `404`-for-missing, which was an existence oracle for other tenants'
  rules. `update_rule` and `delete_rule` never had it — the predicate belongs in the
  statement because a predicate that can be forgotten will be.

  The existing cross-tenant test covered GET, DELETE and the list and **passed**, because the
  buggy handler's answer to a cross-tenant toggle is exactly the answer a guarded endpoint
  should give. The new half asserts the *state* through B's own list rather than the status,
  since "the caller was refused" passes against a build that refuses after writing. Proven
  to fail: with the old `WHERE`, the test returns B's rule body (`cdn-b-rule-0`, `enabled:
  false`) to caller A.

  **That test could not run at all until this tick**, and getting it to run uncovered two
  harness defects in the same file that had been hiding behind each other. `live_state` set no
  CSRF secret, so every mutation in this suite was refused `403 csrf_unavailable` **before its
  handler** — the sixth file with that class, and the same failure mode as tick 81's
  credential field: a suite measuring a 403 reads as a suite proving a product is guarded.
  And the file signed two accounts in per walk against a budget of ten. Both fixed; the
  `Credentials { session, csrf }` + `Deref` type means the twenty-six `&token_a` sites compile
  unchanged while the second cookie travels with the first. **Every mutation in this file is
  now reaching its handler for the first time**, so its green is worth more than it was.

  **`reorderSwapped: false` was the pass, not the panel.** The rule the pass created was rank
  1 on an empty table, and the panel correctly disables "move up" there; the click hit a
  disabled button inside a `.catch(() => {})` and the step asserted on an order nothing had
  been asked to change. The move target is now chosen by position, the step records that the
  button exists and is enabled, and the assertion is tighter — the moved rule exactly one
  position up, length unchanged — because "the first id changed" is also satisfied by a table
  that reversed the whole list.

  `mobileTouchTargets: false` is **not yet established**. The step's comment said "the 44px
  floor" while the code asserted 32, and a screenshot review puts the action row at about
  32px, which is the threshold. The step now records the per-button detail so the next scoped
  pass names which button was short rather than only that one was. The box stays open.

- [ ] The CDN screens pass the browser walkthrough with zero high findings.
  _**Tick 93: the claims that pass would have judged on were not judging anything.** The
  browser gate is still owed — the slot is held by a sibling's live pass — so this tick read
  the two CDN depth passes as source instead of as artifacts, which is the same move that found
  tick 89's `has_credential` and tick 92's retry steps. It found the same defect a third time
  and then a fourth: **`steps.x = …` reaches `summary.json` and is read by no code.**
  `runCdnRulesDepth` had 30 claims of which 13 were gated and `runCdnPurgeDepth` 29 of which 8
  were — and the ungated ones are the load-bearing ones. `reorderSwapped`, `prioritiesDense`
  and `testerIsLive` on the rules screen: a reorder control that moved nothing, a renumber that
  left two rules claiming one priority, and a tester painting "no match" for every sample would
  each have produced a report indistinguishable from a healthy one. `filterNarrows` and
  `countMatches` on the history: a sentence that reads in a summary as *the product's filter
  does not narrow*, when on the tick it was first written the pass was counting the table and
  the mobile card list as separate rows.

  Seventeen and twenty-one claims are now gated. The rules pass reads 87% gated and the history
  93%, with the remainder being raw tallies — a count a reader needs in the report is a fact,
  not an assertion, and gating `rows: 4` would be the same mistake in the other direction.

  `scripts/qa/audit-depth-claims.mjs` now measures this for the whole file instead of by hand,
  so the next writer learns from a one-line command rather than from a fourth incident. It
  reports 146 of 358 claims gated across 27 depth passes, up from 62.

  **And the audit had to be corrected twice before it could be believed**, which is the part
  worth keeping. Its first version credited any identifier in an assignment's text, so the
  string `"/api/v1/webhooks"` contributed the word `webhooks`, which matched the `page:
  "webhooks"` field of every `record(` in that pass: nineteen phantom gates, 243 reported
  against a true 118. Stripping comments fixed a second class. The third was the real one —
  `const steps = {}` makes every claim "reference a local that is read in a guard", because the
  guard reads `steps`, so a pass with two `record()` calls in it came back 100% gated. An audit
  that reports passes nobody took is worse than no audit, because it is believed; the numbers
  now agree with the source.

  The box stays open. `node --check` and `pnpm typecheck` are the only things that have touched
  these lines, which is exactly where tick 89's assertions sat — the first CDN-scoped pass to
  reach the box must read a red here as a real product defect until proven otherwise._
  _**Tick 87: the three `click-error`s this box was waiting on were a real defect in the panel
  frame, not the CDN and not the box.** `/cdn/settings` failed on the navigation links
  `Sessions`, `Devices` and `Search settings`. `app-shell.tsx` renders the sidebar as
  `sticky top-0 h-screen` — one viewport tall — around a flex list with **no `overflow`**, so
  `h-full` clips what does not fit and offers no way to reach it. The probe measures 28 links
  needing 1102px inside a 684px frame, and those three names are **the last three in the
  list**: a race with the pass's own re-inventory does not sort, and that was the story several
  ticks told instead, because the file contains a real `navigated-away` branch for it.

  Nothing about it is visible. The screenshot of the failing moment shows an ordinary sidebar
  with no overlay and no backdrop, and the vision pass has no way to see a control that is
  rendered, positioned, in the DOM, and cannot be clicked. Fixed by making the list the
  scrolling part and the brand and the account block the fixed parts
  (`min-h-0 flex-1 overflow-y-auto overscroll-contain` — `min-h-0` is load-bearing, since a flex
  child defaults to `min-height: auto` and would keep the list clipped *with* a scrollbar's
  appearance). Proven both ways against the live w5 stack at 1440×900: with the fix,
  `scripts/qa/probe-sidebar-reach.cjs` is **5/5** and the admin log shows
  `GET /settings/iam/sessions`, `/settings/iam/devices` and `/settings/search` all answering
  `200`; against `HEAD~1` the same probe reports the three as unreachable. The probe **clicks**
  every link rather than counting them, and `scrollIntoViewIfNeeded` is deliberately not allowed
  to stand in for the click — it would scroll for the probe and report the bug green.

  The box stays open, and the two findings still standing are named rather than waved at. Both
  were resolved this tick, and neither was what the artifact made them look like._

  _**Tick 89: the 500 on `/cdn/rules` was the pass manufacturing it and then reporting it.** The
  admin log carries `GET /cdn/rules 200` thirty-six times across that run and **zero** 5xx, and
  the API log contains no `cdn/rules` request at all — because none reached a server. The depth
  pass proves the error banner by routing a 500 into the browser
  (`page.route("**/api/v1/cdn/rules?*").fulfill({ status: 500 })`), and the roll-up then filed that
  manufactured failure as a high `request-failed` plus a high `console-error` against the screen
  it was measuring. **No registration could have excused it**: the roll-up matched console lines
  on a hardcoded `/status of 40[13]/` and request entries on a hardcoded `[401, 403]`, so a routed
  500 was unreachable by construction — the pass had exactly one way to report its own stimulus.
  Refusals and stimuli are now the same accounting with a **named status set** (`expectRefusal`
  keeps 401/403 as its default, `expectStimulus` registers the 500 it injected, and a genuine 500
  from the API in that window still reaches the report), and all **five** routed 500s in the pass
  are registered, not just this one._

  _**The `/qa-sample` 404 was a check whose subject did not exist.** `SAMPLE_SLUG` is only produced
  by a form filler inside a depth pass the scope may filter out, so a `--only=cdn` run reached the
  public renderer with nothing published and the 404 named the renderer. Gating the check on
  whether some other pass ran was the tempting fix and it is the wrong one — a check that quietly
  skips is indistinguishable in the report from a check that passed, the same camouflage a
  "write-only" assertion gives (see the credential-field note under the purge criterion). The
  renderer now establishes its own precondition: it publishes the page through the **real**
  `POST /api/v1/pages` + `publish` route on the signed-in panel, idempotently, and a failed
  publication is reported as its own finding rather than surfacing later as a 404 pointing at the
  wrong component. It reads the site id with the non-throwing `qaSql` on purpose: `qaScalar`
  throws, and this block sits in a `try` whose `catch` reports `web-unreachable`, so a fixture
  miss would have been filed as "the public renderer could not be reached" — one more finding
  naming the wrong thing._

  _**The product defect this hunt found.** REQ-011 asks for "an error banner with a retry that
  re-runs the failing request", and `/cdn/rules` was the only one of the five screens that
  provoke that state without one — notifications, its preferences, events and webhooks all render
  a `Retry`. Under it the table is empty, and empty reads as "this site has no rules", so a
  transient failure left an operator with no way forward **and** a wrong conclusion. The banner
  now carries a retry wired to the same reload token the Refresh control already bumps. The depth
  pass asserts both halves: `errorOffersRetry` counts the button, and `retryReRunsTheRequest`
  presses it with the route still failing and watches the request go out — counting a button would
  have accepted a label._

  _Gates this tick: `pnpm typecheck` **2/2**, `node --check scripts/qa/walkthrough.cjs` clean,
  `cargo test -p omnion-cdn --quiet` **116/116**. The browser re-run to close the box is queued
  behind two live writers' passes (w3 on :3102, main on :3100), which is a queue, not a blocker._

### QA plan

The walkthrough must visit `/cdn`, `/cdn/purges`, `/cdn/purge`, `/cdn/rules`, `/cdn/rules/new` and `/cdn/settings`, and click: every status card, the primary purge action on `origin` (a real queued purge that finishes), the retry action on a failed item fixture, the rule drag handle, each row action and the bulk actions, the connection test, and the save button on an intentionally invalid TTL (to capture the validation message). Visual check should see: the overview cards render real numbers (not `0` everywhere, not placeholder dashes), the purge table aligns its numeric columns, status badges use the shared badge component, no clipped table headers on the 1280 px viewport, and the mobile pass shows stacked cards rather than a horizontally scrolling table.

### Slices

1. **Rules + headers** — schema for rules/settings, `crates/cdn` matcher, header middleware on the public surface, `/cdn/rules` CRUD with drag reorder. Done: a rule created in the panel changes the `Cache-Control` of a matching public response, and rules persist across restart.
   *Status:* **the screens shipped** (`807e307`, `ee421ad`, `8ca76f0`). `/cdn/rules` is the
   table, the filter, the reorder and the form; `/cdn` is the overview and `/cdn/settings`
   is the provider screen. Three things on the rule form are decisions rather than fields,
   and each is the place a cache configuration usually goes quietly wrong: **the match
   tester** answers while the pattern is typed, because a glob that matches nothing is a
   *valid* rule the API will happily store and the only evidence it is broken is a page that
   did not become cacheable; **the rule order is shown as a rank** and a rule matching
   `/**` is labelled "matches everything" rather than sitting quietly where it looks
   harmless, because the matcher takes the first match and a broad rule above a narrow one
   makes the narrow one unreachable forever; and **the reorder sends the whole list**,
   because a reorder that renumbers one row is what leaves two rules claiming the same
   priority. The mobile rendering is cards carrying the same `data-*` hooks as the rows they
   replace — a hook present in only one rendering silently halves what a depth pass can
   drive, and the mobile measurement is then of a layout no interaction has ever reached.
   The settings screen holds the line that matters most there: the credential is **write-only
   and the form says so**, starting blank on every visit and omitted from the payload unless
   the operator typed something — an empty field that looked like "no key stored" sends
   somebody to re-enter a working key, and one that rendered the saved key is a leak.
2. **Purge pipeline** — purge tables, provider adapters, purge console, worker drain with retry, history and detail drawer. Done: a manual purge of one URL and one tag reaches `succeeded`, and a forced adapter failure retries then lands `failed` with the message visible.
   *Status:* **the code and the walks are done; the browser gate is still queued.** The queue
   (`0054`), the crate's decisions (`omnion_cdn::purge`), the nine routes, the drain worker and
   both screens shipped this tick, proved by **17/17** integration walks against
   `omnion_qa_w5`. Both halves of the slice's own "Done" sentence are asserted: a URL purge
   and a tag purge reach `succeeded` with every item `done`, and a forced adapter failure
   retries once — with the intermediate state checked, because "fails on the first refusal"
   and "retries" are different products — then lands `failed` with the provider's message on
   the item, on the parent row and in the drawer. The retry moves only the failed items.
   **The `runCdnPurgeDepth` browser pass has not run yet** (the shared QA slot is held by
   another writer), so this slice is not closed.

   The bug the walks found is the note worth keeping from this slice: `claim_due`'s RETURNING
   list was unqualified against a CTE that exposes a column called `id`, and PostgreSQL
   refused the statement at runtime on the first item the worker ever claimed. It compiles,
   passes clippy, and is a string — so the drain shipped unable to do its one job with every
   unit test green. The only thing that found it was a walk that runs the statement.
3. **Automatic invalidation** — event subscription for publish/unpublish/media/theme/domain, trigger toggles in settings. Done: publishing a page from the panel queues a purge automatically and the new version is served after it completes.
   *Status:* **the mechanism shipped; the walkthrough pass on it is still queued.** The trigger
   table, the planner, the cursor walk (`0120`), the provenance table and the drawer's
   "automatic · `page.published` · event 412" line are in (`3603060`), and the settings screen's
   trigger list now names **seven real events instead of six, two of which were never emitted at
   all**. That correction is the substantive part of this slice and it was not in the plan: REQ-011
   names `media.replaced` and `site.domain.changed`, and nothing in the platform records either —
   a file's bytes are replaced by `media.version_created`, and a domain changing is `domain.added`
   or `domain.removed`. Two of the six switches on `/cdn/settings` could be turned on, saved,
   and observed doing nothing for ever, with no error anywhere. The trigger table consumes the
   names the bus carries, and a test in the crate asks `omnion_events::catalogue` whether every
   one of them exists, so the next rename is a failing build rather than a quiet regression.

   The other two decisions this slice made are in `invalidation`'s module docs because the
   obvious implementation is wrong in a way that only shows up in production: an automatic purge
   is written with `requested_by = null` (borrowing the publisher's id would make the history
   accuse a person of a decision the platform took, and the drawer now names the event instead);
   and a whole-site trigger against a provider that cannot hold surrogate keys resolves to the
   site's published addresses **at plan time**, because a `site-<uuid>` tag handed to
   `generic_http` is a body no endpoint reads — the adapter answers `Succeeded` and the operator
   has a successful purge that invalidated nothing.
4. **Provider + settings depth** — adapter catalogue, masked credentials, `generic_http` signed payload, `cdn.purge.failed` webhook, counters on the overview. Done: a test endpoint receives a correctly signed purge payload and the overview counters reflect it.
   *Status:* **the credential half shipped this tick** (`a7169471`), and what it found is the
   note worth keeping: the field had been a **button reporting success** since slice 1. The
   panel rendered it, sent it, labelled it write-only — and the handler dropped the value with a
   comment saying the decrypt "is slice 4". Every settings walk asserted
   `has_credential == false` to prove the API does not leak a credential, and that assertion is
   *satisfied by a build that never stored one*: a write-only guarantee and a missing
   implementation produce the same green, which is why three REQs' worth of tests never
   noticed. Four walks now (`a7169471`): the end-to-end one resolves the credential back out of
   `provider_for_site` — the function the drain calls, not a copy — and reads
   `authorization: bearer …` off a loopback socket after a real purge; the keep-and-replace one
   covers the two other states a form can be in; the refusal one names the field and checks
   nothing was written; the audit one asserts the trail says `credential_replaced: true` and
   does not contain the value. Suite **20/21**, the 21st a shared-Postgres `PoolTimedOut` that
   passes alone.

   Two of the decisions are not the obvious ones. The envelope is the platform's existing
   `SecretBox` rather than a dependency added for one column, because a second answer to "how
   is a stored secret protected" means a key rotation that has to know about both. And an
   **unreadable** envelope is a 500 named `CredentialUnreadable` rather than a 400
   `invalid_credential`: the stored value is fine and this process holds the wrong key, and
   telling the operator their credential is invalid would send them to paste the key they have
   already pasted.

   **(tick 91: the counters half was never proved, and the walk that claimed it could not have
   failed.)** The slice's "Done" sentence has two legs — a signed payload to a test endpoint,
   and "the overview counters reflect it". The first ships. The second was covered by
   `the_overview_reports_the_queue_the_counters_and_the_last_twenty`, which queues two purges,
   drains **neither**, and asserts `failure_rate == 0.0` — a value that is *structurally* zero
   when nothing has been attempted. A card wired to a constant passes that walk, which is the
   whole failure: the assertion and the defect it was meant to catch are indistinguishable.

   `the_overview_counters_follow_a_real_drain_rather_than_the_queue` (`2c0ff769`) drives the
   window through two real drains and reads the counters back off the API. **The order is the
   whole trick and it is not a style choice:** `cdn_settings` is scoped to the *site*, not to a
   purge, so a row written before the first drain sends the success case at the dead endpoint
   as well — the walk I wrote first failed exactly that way, measuring two failures. The first
   drain runs against the default `origin`; only then does the `generic_http` row exist, with
   `max_attempts = 1` so the refusal is a **failure** on the first pass rather than a retry. A
   two-attempt budget would leave the item `pending`, the counters would read 1 succeeded / 0
   failed, and that looks like a working card without being one.

   The pre-drain zeros are asserted for the same reason: without them, an `succeeded_24h` of 1
   is also consistent with a counter that ignores the drain and counts the queue twice.

   `on conflict (site_id)` cannot be used against `cdn_settings` at all, and the error is worth
   keeping: uniqueness there comes from a **partial unique index** (`where site_id is not null`),
   not a constraint, so the column list matches nothing and PostgreSQL answers `42P10`. The row
   is deleted and reinserted — which also makes the walk's premise explicit rather than
   depending on what the fixture happened to leave behind.

### Risks / notes

- Credential handling is the sharp edge: values are write-only, stored encrypted, never returned by the API and never written to audit payloads or event payloads. The repo must stay free of any provider key.
- A CDN adapter that half-works is worse than none: every shipped adapter needs a reachability check and an honest error path; unimplemented providers are simply absent from the catalogue.
- Tag-based purging depends on the CDN supporting surrogate keys; `origin` and `generic_http` fall back to URL purges derived from the same tag map, and the UI says which strategy ran.
- Purge storms: cap enqueued items per trigger and coalesce URLs that repeat within a short window.
- Public cache headers must not leak private data: only published content routes are cacheable, and anything behind a session sends `Cache-Control: private, no-store` as today.


**Correction (tick 103).** Two names in this request were ones the platform has never emitted and never will under those spellings, and a consumer built to this spec would have subscribed to nothing: `media.replaced` is emitted by the file manager as `media.version_created` (REQ-010 names it in its own Events list and in the comment on the emitter), and the tenancy layer emits `domain.added` / `domain.removed` rather than a single `site.domain.changed`. Both lines above — the scope list and the event contract — now say the way the emitters actually record them. The gate `every_consumed_name_in_a_live_area_is_deliverable` (`apps/api/tests/events.rs`) is what found them: it walks every `Consumed:` line in `docs/requests/` and fails when the name's area already emits other events, but this exact name is neither live nor `Reserved`. It is the third direction of the same seam the two existing drift gates close — those hold emitters against the catalogue, and this holds the **specs** against the catalogue.
