# REQ-011 — CDN / Edge System

> **Status:** in-progress (`ca65085`) · **Captured:** 2026-09-25 · **Layer:** platform / infra
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
- Automatic purge triggers: `page.published`, `page.unpublished`, `page.deleted`, `media.replaced`, `theme.activated`, `site.domain.changed` — each mapped to URLs and tags by the rule set.
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

**Emitted:** `cdn.purge.requested`, `cdn.purge.completed`, `cdn.purge.failed`, `cdn.rule.changed`, `cdn.settings.updated` — each with the identifiers a consumer needs (purge id, site id, kind, counts) and no rendered content. **Consumed:** `page.published`, `page.unpublished`, `page.deleted`, `media.replaced`, `theme.activated`, `site.domain.changed`; a consumed event maps to URLs and tags through the rule set, then enqueues one purge.

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
- [ ] Purge by URL list runs end to end and the history row reaches `succeeded` with per-item results.
- [ ] Purge by tag and "everything" both work; "everything" requires the typed confirmation.
- [ ] Publishing a page produces a purge row automatically within one worker tick, honouring trigger toggles.
- [ ] A provider error marks the purge `failed`, records the provider message and leaves items retryable.
- [ ] Retry from the history drawer requeues only failed items and updates the counts.
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
- [ ] Purge console rejects more than 500 targets and an invalid URL with a clear message.
- [x] Every mutation writes an audit entry under the `cdn.*` namespace with actor and IP. (`0e2993c`)
- [x] All endpoints are guarded by the catalogue keys and a forbidden call returns `403 permission_denied`. (`0e2993c`)
- [ ] Filters, empty, loading and error states exist on every screen; the rows shown match the API counts.
- [ ] The CDN screens pass the browser walkthrough with zero high findings.

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
3. **Automatic invalidation** — event subscription for publish/unpublish/media/theme/domain, trigger toggles in settings. Done: publishing a page from the panel queues a purge automatically and the new version is served after it completes.
4. **Provider + settings depth** — adapter catalogue, masked credentials, `generic_http` signed payload, `cdn.purge.failed` webhook, counters on the overview. Done: a test endpoint receives a correctly signed purge payload and the overview counters reflect it.

### Risks / notes

- Credential handling is the sharp edge: values are write-only, stored encrypted, never returned by the API and never written to audit payloads or event payloads. The repo must stay free of any provider key.
- A CDN adapter that half-works is worse than none: every shipped adapter needs a reachability check and an honest error path; unimplemented providers are simply absent from the catalogue.
- Tag-based purging depends on the CDN supporting surrogate keys; `origin` and `generic_http` fall back to URL purges derived from the same tag map, and the UI says which strategy ran.
- Purge storms: cap enqueued items per trigger and coalesce URLs that repeat within a short window.
- Public cache headers must not leak private data: only published content routes are cacheable, and anything behind a session sends `Cache-Control: private, no-store` as today.
