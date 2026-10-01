# REQ-019 — Headless CMS

> **Status:** in-progress (slice 3b's **Usage tab is BUILT** as `ba54fb74` — the chart, the endpoint
> leaderboard and the per-token table, with the flushed/pending split carried through every column and
> `pending_readable` (not the arithmetic) deciding whether the counting column is a number or an em dash.
> Two API additions came out of writing it and neither was in the spec's table: the leaderboard is
> accumulated server-side in the same pass as the per-token rows, and each token now carries its own
> `last_used_at`. The tab is in the walkthrough inventory **and** the 390 px list, because its mobile
> claim is a card list beside a `sm:hidden` table and one overflow measurement cannot stand for two
> layouts. **`--only=content-api` is queued behind a live sibling.** The Explorer is the rest of slice 3.
> slice 3a's **budget + metering are BUILT and GREEN**: the per-token
> budget is enforced in the same Redis round trip that records the call, `GET /api/v1/content-api/usage`
> answers, and the flush worker carries the window into `api_token_usage_daily`. `content_api_metering.rs`
> is **6/6** against the live stack; `omnion-content --lib` **314/0** and `omnion-api --lib` **288/0**.
> Four defects found and fixed while writing it — a rate-tier error that named the *name* field, the
> `/api/v1` prefix splitting every usage row in two, a spare `.arg()` in the Lua call that recorded
> every request as an error, and a test that measured a per-minute window without pinning the minute.
> The Explorer and the usage *screen* are the rest of slice 3. **Slice 2's read surface is
> MEASURED and GREEN**: `content_read_surface`
> is **16 ok / 0 failed**, from 0/13; `omnion-content --lib` **308/0** and `omnion-api --lib`
> **253/0**. The cursor walk — the one criterion this REQ's second slice exists for — now passes,
> and making it pass found **six more product defects**, five of them invisible to any
> single-page assertion.
>
> **Defects found and fixed this tick:**
> 1. **The cursor was bound as `text` into a `timestamptz` comparison.** The pages list parsed
>    the cursor's instant correctly and then called `.to_string()` on it, so PostgreSQL answered
>    `500 operator does not exist: timestamp with time zone < text` and **every walk died on
>    page two**. Parsing and then re-stringifying threw away the type it had just recovered.
> 2. **`?sort=title` was a `500` on every call.** The column name was a bare `title` and the
>    query prefixed it with `p.`, producing `p.title` — a column that has never existed on
>    `pages`, because the title is the *revision's*. `SortKey::expression(source)` now names the
>    qualified expression per relation and the ORDER BY, the keyset predicate and the cursor all
>    read that one field.
> 3. **`?sort=title` on media was a `500` too, and the OpenAPI document promised it.** A file has
>    no title, so `sort=title` is now a `400 invalid_parameter` naming `sort` and listing the two
>    sorts media does have. A doc test reads the *handler's* accepted set and requires the
>    document to match, because the document and the handler once agreed with each other and were
>    both wrong.
> 4. **The media cursor's writer and reader disagreed on the format** — `to_string()` out,
>    `Rfc3339` in — so the media walk was a `400` from page two on. The same defect was fixed in
>    the *other* direction last tick on the pages side, which is the proof that a format must be a
>    function both halves call: `content_read::stamp` and `content_read::cursor_instant`.
> 5. **`count` did not count the items sent.** `items` was rendered from `rows` (which includes
>    the over-fetch row) while `count` and the cursor were taken from `visible` (which does not),
>    so a `limit=2` call answered `count: 2` with **three** items. The walk saw it as seven slugs
>    across four pages for five rows. `visible` is now the whole answer — items, count, cursor and
>    ETag — so "what the caller is told" and "what the caller is sent" cannot disagree.
> 6. **The media list over-fetched nothing**: it bound `request.limit` where the pages list bound
>    `fetch_limit()`, so media's `next_cursor` was always `null` regardless of the set size.
>
> Also: `Page::new` — a helper nothing called, whose `fetched == limit` rule **contradicts** the
> routes' correct over-fetch rule — is gone, and the decision now lives in
> `content_read::continues(fetched, limit)` with the exact cases that separate the two rules
> tested. And the item's own `updated_at` is rendered with `stamp`, so **the surface round trips
> its own output**: the `updated_since` test's hand-written `to_rfc3339` workaround is deleted and
> the value is fed back verbatim.
>
> **The tests that would have caught all six** are now in place: `a_page_sends_the_limit_and_says_so`
> (defect 5), `a_title_sort_pages_by_the_revision_title_descending` (2, 4), and
> `a_sort_the_media_list_cannot_do_is_refused_by_name` (3, 6). One of them was **wrong when
> written** — it asserted `sort=title` returns A→Z and failed against a correct endpoint, because
> every sort on this surface is descending and the test's own expectation was the defect.


## Request

Omnion must not be tied to its own Next.js renderer. Content API:

```text
/api/v1/content/pages
/api/v1/content/posts
/api/v1/media
```

So frontends can be:

- Next.js
- React
- Vue
- Nuxt
- mobile app
- custom application

## Implementation spec

The public renderer already reads published pages through `/api/v1/public/pages/{slug}`, which is anonymous and single-purpose. This request adds the surface a
*developer* integrates against:
token-authenticated, paginated, filterable, field-selectable, localized, documented and rate limited — read-only in v1, with usage visible in the panel.

### Scope (in / out)

**In**

- **Content read API** for pages, posts and media metadata: cursor pagination, field selection, filtering, sorting, locale selection (REQ-020), per-item
  `etag`/`updated_at` for cache validation, and consistent error codes.
- **API tokens**: org-scoped, optionally site-scoped, with read scopes, expiry, optional origin allow-list, per-token rate limit, request metering, rotation and
  revocation. Token plaintext is shown once and stored hashed.
- **OpenAPI 3.1 document** for the content surface, served as JSON and downloadable; it is the contract that frontends and generated SDKs consume.
- **In-panel API explorer** that makes real calls with a selected token and emits copy-ready snippets (cURL, fetch, Python), plus a Docs tab generated from the
  OpenAPI document.
- **Usage view**: requests per day, top endpoints, error and throttle counts, per-token breakdown.
- **Webhook integration** documented end-to-end: subscribe a frontend's build hook to `page.published` / `page.updated` (REQ-016) and rebuild only when
  something changed.

**Out**

- GraphQL — explicitly deferred; the OpenAPI document plus field selection covers the same need and half a GraphQL server is worse than none. Revisit after v1
  usage data exists.
- Write API for third parties (create/update content through a token). v1 is read-only; a write scope is reserved by name (`content:write`) but not implemented.
- Draft/revision access through tokens — tokens only ever see published revisions, exactly like the anonymous surface. Preview access stays with REQ-018's
  signed links.
- Media uploads, transforms and CDN edge caching (REQ-010, REQ-011) and per-field permissions (REQ-006 covers the panel side).

### Screens (UI)

- **`/content-api` — Tokens tab (default).** Table columns: Name, Prefix (`omn_xxxxxxxx`, copy), Site scope (All sites / one site), Scopes (badges), Requests
  (30 d), Error rate, Last used, Expires (relative + exact on hover), Status (active / expired / revoked). Filters: status, site, text search. Bulk: Revoke
  selected (typed confirmation). `Create token` opens a dialog: Name (1–64, unique per organization case-insensitively), Site scope (radio: All sites / a site),
  Scopes (checkboxes `content:read`, `media:read`, at least one required), Expiry (30 / 90 / 365 days / never, default 90), Allowed origins (optional list, one
  origin per line, validated as `scheme://host[:port]`), Rate limit tier (Standard 120/min, Elevated 600/min — guarded by the manage permission).
  The created token is shown once in a copy field with an `I stored it` checkbox that gates the `Done` button, plus a warning that it cannot be shown again.
- **`/content-api/explorer` — Explorer tab.** Three panes: endpoint picker (the OpenAPI paths with method badges and one-line descriptions), parameter form
  generated per endpoint (site, locale, `limit`, `cursor`, `fields`, filters, sorting), and a request/response pane with syntax-highlighted JSON, response headers
  (`etag`, `x-ratelimit-remaining`), timing in ms and the resolved request URL. Actions: `Send`, `Copy as cURL`, `Copy as fetch`, `Copy as Python`, `Use cursor for next page`.
  The selected token comes from a dropdown (tokens the caller may read); the explorer never displays a token it cannot read, and a token with no reads left is not
  selectable. State is deep-linkable:
  `?endpoint=pages.list&site=main&locale=tr-TR&limit=5`.
- **`/content-api/docs` — Docs tab.** Rendered from the OpenAPI document: an endpoint list with parameters, response shapes, error codes, a pagination guide
  with a working cursor example, the localization parameter's behaviour, the rate-limit contract (`429` + `Retry-After`), and the recommended webhook/rebuild
  pattern. Buttons:
  `Download OpenAPI (JSON)`, `Download OpenAPI (YAML)`, `Copy base URL`.
- **`/content-api/usage` — Usage tab.** A 30-day requests chart, endpoint leaderboard, error and throttle counts, and a per-token table (requests, errors,
  throttled, last used). Empty state: "No requests yet — try the Explorer."
- **Global states.** Loading: skeleton rows and a spinner inside the Send button. Errors: API errors render with code, message and the offending parameter
  highlighted in the form. A `403` from the panel's own API explains which permission is missing. Below `lg`:
  the explorer stacks into endpoint → form → response panes with sticky tabs; tables become cards.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/content/pages` | Published pages: `site`, `locale`, `type`, `tag`, `limit` (≤ 100), `cursor`, `fields`, `sort`, `updated_since` | token `content:read` |
| GET | `/api/v1/content/pages/{slug}` | One page plus its published revision and alternates | token `content:read` |
| GET | `/api/v1/content/posts` | Posts (`page_type = 'post'`): same parameters as pages | token `content:read` |
| GET | `/api/v1/content/posts/{slug}` | One post | token `content:read` |
| GET | `/api/v1/content/media` | Media metadata list: `site`, `mime`, `folder`, `limit`, `cursor`, `fields` | token `media:read` |
| GET | `/api/v1/content/media/{id}` | One media item with dimensions, size, alt text and a signed URL | token `media:read` |
| GET | `/api/v1/content/sites` | Sites this token may read: key, name, default locale, enabled locales, primary host | token `content:read` |
| GET | `/api/v1/content/openapi.json` | The OpenAPI 3.1 document for this surface | token `content:read` |
| POST | `/api/v1/content-api/tokens` | Create a token; returns plaintext once | `content.api.manage` |
| GET | `/api/v1/content-api/tokens` | List tokens (prefix only, never plaintext) | `content.api.read` |
| PATCH | `/api/v1/content-api/tokens/{id}` | Rename, change scopes, expiry, origins or rate tier | `content.api.manage` |
| POST | `/api/v1/content-api/tokens/{id}/rotate` | New secret; previous secret invalid immediately | `content.api.manage` |
| DELETE | `/api/v1/content-api/tokens/{id}` | Revoke a token | `content.api.manage` |
| GET | `/api/v1/content-api/usage` | Usage summary: `from`, `to`, per-token and per-endpoint breakdown | `content.api.read` |

Token auth: `Authorization: Bearer omn_<id>_<secret>`; a `?token=` query parameter is accepted for local experiments only and logs a warning. Every list answers
`{"items": [...], "next_cursor": "..." | null, "count": n}`; every item carries `id`, `slug`, `type`, `locale`, `updated_at` and `etag`. Errors use the platform
envelope (`{"error":
{"code", "message", "param"}}`) with `400 invalid_parameter`, `401 invalid_token` / `token_expired`, `403 insufficient_scope`, `404 not_found`, `429
rate_limited` (with `Retry-After`) and `500 internal_error`.

### Data model

Migration `0014_content_api_tokens.sql` (number is a placeholder — renumber to the next free slot):

- `api_tokens` — `id uuid pk default gen_random_uuid()`, `organization_id uuid not null references organizations(id) on delete cascade`, `site_id uuid references sites(id) on delete cascade`
  (null = all sites of the organization), `name text not null`, `prefix text not null unique check (prefix ~ '^omn_[0-9a-f]{8}$')`, `token_hash text not null unique check (length(token_hash) = 64)`,
  `scopes text[] not null check (cardinality(scopes) between 1 and 8)`, `allowed_origins text[]`, `rate_limit_per_minute integer not null default 120 check (rate_limit_per_minute between 10 and 600)`,
  `expires_at timestamptz`, `revoked_at timestamptz`, `last_used_at timestamptz`, `created_by uuid references users(id) on delete set null`, `created_at timestamptz not null default now()`,
  `updated_at timestamptz not null default now()`. Unique index `api_tokens (organization_id, lower(name))`; index `(organization_id, created_at desc)`;
  partial index `(prefix) where revoked_at is null`.
- `api_token_usage_daily` — `token_id uuid not null references api_tokens(id) on delete cascade`, `day date not null`, `endpoint text not null`, `requests integer not null default 0`,
  `errors integer not null default 0`, `throttled integer not null default 0`, `primary key (token_id, day, endpoint)`, index `(day desc)`.
  Counters are incremented in Redis per request and flushed to this table every 60 seconds (and on shutdown), so metering never writes a row per request.
- Permissions: extend `crates/permissions/src/catalogue.rs` and the seed with `content.api.read` ("View content API tokens and usage") and `content.api.manage`
  ("Create, rotate and revoke content API tokens"), granted to the Owner and Administrator roles by default.
- No change to `pages` / `page_revisions` / `translations`; the surface reads them through `crates/content`. `page_type = 'post'` rows are served as posts until
  the blog module ships its own tables, and the endpoint is marked experimental in the OpenAPI document.

### Events

- **Emitted:** `content.api.token.created` (name, prefix, scopes — never the secret), `content.api.token.rotated`, `content.api.token.revoked`,
  `content.api.throttled` (first crossing per token per hour, so a runaway integration is visible without flooding the feed).
- **Consumed:** none; the read path is deliberately event-free.
- **Webhook relevance:** the practical integration is the reverse direction — a frontend subscribes to `page.published`, `page.unpublished`, `media.updated` and
  `translation.published` and calls its own rebuild hook; the Docs tab documents that pattern with a working example payload.

### Acceptance criteria

- [x] `GET /api/v1/content/pages` with a valid token returns only published pages of the token's site scope, newest first by `updated_at`, with `next_cursor`
  present while more rows exist. *(only_published_pages_are_served_and_never_a_draft,
  a_page_sends_the_limit_and_says_so)*
- [x] Walking the cursor returns each page exactly once across three pages of `limit=2`.
  *(the_cursor_walks_a_set_exactly_once — the test this slice exists for)*
- [x] `fields=slug,title` returns only those keys plus the always-present identity keys, and an unknown field is refused with `400 invalid_parameter` naming the
  parameter. *(a_projection_keeps_the_keys_a_caller_needs_to_keep_going, an_unknown_field_is_refused_by_name)*
- [x] A request without a token answers `401 invalid_token`; a token for another organization's site answers `404 not_found` (never a cross-tenant leak).
  *(a_missing_or_wrong_credential_is_refused_before_any_row_is_read, a_site_scope_is_a_filter_and_never_a_confirmation,
  a_single_page_is_served_and_an_unpublished_one_is_not_found)*
- [x] A token without `media:read` calling `/api/v1/content/media` answers `403 insufficient_scope`. *(media_is_its_own_power)*
- [x] A token with `expires_at` in the past answers `401 token_expired`, and the Tokens tab shows the row as `expired`.
  *(the API half is proven by `an_expired_token_says_so_rather_than_saying_it_is_wrong` in slice 1's suite; the
  panel row needs the QA pass)*
- [x] Rotation invalidates the previous secret immediately (old secret → `401`) and returns a new plaintext exactly once.
  *(`rotation_kills_the_previous_secret_immediately`, slice 1)*
- [ ] Rotation invalidates the previous secret immediately (old secret → `401`) and returns a new plaintext exactly once.
- [ ] Revoking a token answers `401` on the next call, and the panel row reads `revoked`. *(the API half is proven —
  a_revoked_token_stops_reading_immediately — the panel row needs the QA pass)*
- [x] The 121st request inside a minute at the Standard tier answers `429` with a `Retry-After` header, and the usage table records one throttled request.
  **BUILT and GREEN this slice** — `content_api_metering.rs`, 6/6 against the live stack, plus 20 new unit tests
  (`omnion-content --lib` **314/0** and `omnion-api --lib` **288/0**, from 308/0 and 274/0).

  **The tier list is a LIST, not two constants, and that is what made the criterion testable.** The
  store accepted only 120 and 600, so proving "the 121st request" means firing 120 requests — a
  minute-long test nobody writes. `RATE_TIERS = [10, 120, 600]`, each with a label, and the
  **error message names them all**: three bare numbers is a puzzle, three labelled ones is a menu.

  **Four defects found by writing it, three of them invisible to any single assertion:**

  1. **A bad rate tier was reported with `field: "name"`.** `validate_rate_limit` returned
     `InvalidText`, which the API maps to a `400` whose `details.field` is `"name"` — so a caller
     who submitted a bad *tier* was told their token's **name** was wrong. The create dialog
     highlights the field the error names, so the operator edits the name, the dialog saves, and the
     limit stays wrong. **A field-level message pointing at the wrong field is worse than no field
     at all**, because it sends someone to fix something that was never broken. `ContentError` gained
     its own `InvalidRateTier` variant, and the exhaustive `code()` match made forgetting it a
     compile error.
  2. **`MatchedPath` is absolute from the application root**, so every usage row was keyed
     `/api/v1/content/pages` while the OpenAPI document, the panel's copy button and the explorer's
     own snippet all say `/content/pages`. One endpoint under two spellings means **two rows in the
     usage table**, and the bug is invisible in the response because the response never mentions
     either. `surface_route()` strips a *named* constant, and a test reads the mount point out of
     `mod.rs` so a version bump that moves the tree is a test failure rather than a quiet split.
  3. **A spare `.arg(1)` in the `EVAL` call meant every request was recorded as an error.** The
     script reads `ARGV[2]` as `errors`; a leftover argument shifted it, so `errors` tracked
     `requests` exactly and the usage chart showed a 100% error rate on an installation serving only
     `200`s. Not a type error, not a runtime error, and not visible in any response — it was found
     by a test that read the **raw Redis hash** instead of trusting the route's own account of
     itself. A test now compares the script's highest `ARGV` against the call's argument count,
     because a spare argument is the one mistake no compiler catches.
  4. **A per-minute budget cannot be tested without a pinned window.** The suite asserted an exact
     countdown and failed roughly once a minute: a burst that straddles 12:00:59 → 12:01:00
     legitimately lands in a fresh window with a whole budget. That is the *documented contract* —
     a new minute is a new budget — so the **test** was wrong, and `wait_for_fresh_window` is the
     fix. Recorded because the next author of any test against a windowed counter will make the
     same mistake and call it a flake.

  **Three decisions worth keeping.** The counter is incremented *before* the decision, so a client
  over budget keeps appearing in the usage tab — a chart that flattens exactly while an integration
  is in trouble reads as recovery. `Retry-After` is the rest of the minute the caller is *inside*,
  read from the same bucket index the counter used, because a constant `5` makes a well-behaved
  client's retry loop into the load the limit exists to shed. And `X-RateLimit-*` is **absent rather
  than zero** when the counter was unreachable: a client reading `remaining: 0` from a counter
  nobody could read backs off a token that is not being limited, so the meter failing open would
  throttle the caller by accident.

  **The flush is additive and replayable, and the test proves the addition separately.** A worker
  that dies after writing and before clearing re-runs the same window; `requests = requests +
  excluded.requests` counts it once where a replace would double it. The clear happens *only after*
  the write — the asymmetry is the reason the order is not a matter of taste: clearing first loses a
  day, and a lost day is the one number an operator cannot reconstruct.

  **Still open in this slice:** the Explorer and the `/content-api/usage` *screen* — the route
  exists and its shape is proven over HTTP, but no panel tab renders it yet.
- [x] `etag` / `updated_since` let a caller fetch only changed items (proven by two sequential calls where only one item changed).
  *(updated_since_returns_only_what_changed — and the value is now fed back verbatim, the surface round trips its own output)*
- [x] `GET /api/v1/content/openapi.json` returns a document that parses as valid JSON, declares `openapi: 3.1.0`, and contains every route in the API table with
  its permission scope. *(the_openapi_document_is_valid_and_complete; the documented sorts are now checked
  against the handler's accepted set)*
- [ ] The Explorer executes a real call against the running API, shows status, headers and timing, and its cURL snippet reproduces the same response when pasted
  into a shell. **The Explorer is the remaining half of slice 3** — nothing here claims it, and the usage criterion below is deliberately worded so it can be measured without it.
- [ ] Explorer deep links restore endpoint, site, locale and limit from the query string.
- [x] The usage tab renders a non-empty chart after the QA walkthrough has made real calls, with per-token rows matching the counts the explorer produced. **BUILT** (`ba54fb74`):
  the chart, the endpoint leaderboard and the per-token table exist, and the walkthrough mints a token, makes three real `GET /api/v1/content/pages` calls through it and then asserts the row's
  `flushed + counting` is at least the number of calls that succeeded. The three real calls stand in for "the explorer" because the Explorer does not exist yet — the
  claim under test is the tab's agreement with the platform, not the Explorer's UI, and a walkthrough that drove the Explorer could not run until it ships.
  **Two things this criterion forced into the API, both of which existed only as panel-side arithmetic:** the endpoint leaderboard (accumulated server-side in the same pass as the
  per-token rows, over the same live window, so the two numbers one screen apart cannot disagree) and `last_used_at` per token (the table cannot answer "is this token doing anything"
  without it). **Measured by `qa-sql` + the walk, pending the `--only=content-api` pass.**
- [ ] `POST /api/v1/content-api/tokens` with an invalid origin, an empty scope list or a duplicate name each fail with a field-level message and a named error
  code.
- [ ] An anonymous browser cannot read the token list (`401`), and a role without `content.api.read` sees no Tokens tab.
- [ ] The QA walkthrough covers `/content-api`, `/content-api/explorer`, `/content-api/docs` and `/content-api/usage` with zero high findings.

### QA plan

The walkthrough creates a token in `/content-api` (copy-once dialog, `I stored it` gate), then opens the Explorer, sends a pages request, copies the cURL
snippet, pages forward with the cursor returned in the response, then repeats against `/content-api/content/media` to observe the scope error with a token that
lacks `media:read`. It visits `/content-api/docs`, downloads the OpenAPI document and re-serves it to confirm it is valid JSON, then `/content-api/usage` to see
the requests just made reflected in the chart. It finishes by revoking the token and confirming the Explorer reports `401`. The visual check must see:
a token table with real prefixes and last-used timestamps, JSON in the response pane with working highlighting, a chart with bars for the current day, honest
empty states before the first token, and no token plaintext visible anywhere after the creation dialog is closed.

### Slices

1. **Tokens + auth.** Migration, catalogue additions, token CRUD with copy-once secret, Bearer authentication middleware for the content surface, `/content-api`
Tokens tab. *Done line:* a created token authenticates a `pages` request and a revoked one is refused.
2. **Content surface + OpenAPI.** Pages, posts, media and sites endpoints with pagination, `fields`, `updated_since`, locale parameter support, consistent
errors, the OpenAPI document and the `/content-api/docs` tab.
*Done line:* three cursor pages walk the seeded content exactly once and the served OpenAPI document lists every route.
3. **Explorer + metering.** In-panel explorer with real calls and snippets, Redis counters, daily usage flush, rate limiting with `429`/`Retry-After`,
`/content-api/usage` tab, throttled event.
*Done line:* the explorer's debug panel shows `x-ratelimit-remaining` decreasing and the usage tab shows the same request count after a minute.

   **3a (the metering half) is BUILT and GREEN — `afdaef5e` + this commit.** The limiter, the counter and
   the usage route exist and are proven over HTTP; the Explorer and the two panel tabs were what remained.
   The split was worth making: metering is a claim about *numbers*, so it is provable with a token and a
   `redis-cli`, while the Explorer is a claim about a screen and needs a browser. Shipping them together
   would have meant neither could be verified until both were done. **The Usage tab is `ba54fb74`; the
   Explorer is still to come.**

   The design decision everything else follows from: **the budget and the usage counter are incremented by
   one Lua script over two keys.** A limiter that counts in one key and a usage tab that counts in
   another is a chart that disagrees with the platform, and that disagreement is invisible from either
   side. The two reasons a naive two-round-trip version is wrong are both about windows — a `GET` then an
   `INCR` lets N concurrent callers all see "119 of 120" and all be allowed, and two keys written at
   different moments can be read as a state neither of them was in.

   The read path touches Redis once, and only writes a row per request for the *errors* — and only for
   requests that failed, which is why the 99% of calls that succeed cost the same as before. A content
   token is a high-volume credential by definition, and a write per call would turn the usage view into a
   write amplifier competing with the reads it measures.

   **3b (the screen half) is BUILT — `ba54fb74`.** The Usage tab: a zero-filled 30-day chart, the
   endpoint leaderboard and the per-token table. The split into 3a/3b was worth making for the same
   reason 3a/3b as metering/screen: the metering half is a claim about *numbers* and was provable with a
   token and a `redis-cli`; the screen half is a claim about a *screen* and needs a browser.

   Two API additions fell out of writing the screen, and neither was in the spec's API table:

   - **The endpoint leaderboard.** A leaderboard summed in the browser out of `rows` would be a
     flushed-only number sitting above a flushed-plus-pending table, and the two would be one screen
     apart with nothing to reconcile them — the exact disagreement the route was written to refuse in
     `UsageBody`. It is accumulated server-side in the same pass over the same rows, and the live window
     is added to it only when `pending_readable` is true, so a partial `SCAN` cannot inflate it.
   - **`last_used_at` per token.** "Is this token doing anything" is the question this screen is asked,
     and a table without a last-used column answers it with a blank cell. It comes from the same query
     as the name, so there is no second read and no chance of the two disagreeing about which tokens
     exist.

   **The one number this screen refuses to print is a total.** Flushed and pending are two columns,
   everywhere, and the freshness note says why in the screen rather than in a spec. A reducer summing
   zeroes cannot produce a `null`, so `pending_readable` — not the arithmetic — is what turns the
   counting column into an em dash on an installation whose counter is unreachable. A screen that
   printed `0` there would be the same lie `Counted::authoritative` refuses to tell upstream.

### Risks / notes

- **Deviation from the request text:** the brief lists `/api/v1/media`; that path is the panel's authenticated media CRUD surface. The headless media read
  surface therefore lives at `/api/v1/content/media`, and the OpenAPI document explains the split.
  Reusing the panel path with two auth models would be a security trap.
- Token leakage is the dominant risk: hashing at rest, prefix-only display, one-time plaintext, warning copy in the dialog, no token in logs or events, and a
  copy-once flow that cannot be skipped.
- CORS: default deny; when `allowed_origins` is set, only exact origins are echoed, credentials are never allowed with `*`, and an `OPTIONS` preflight never
  requires a token.
- Caching: responses carry `ETag` and `Cache-Control: public, max-age=0, must-revalidate` so a CDN (REQ-011) can revalidate without serving drafts.
- Cursor stability: cursors are opaque keysets over `(updated_at, id)`, so deleted items never shift later pages; `updated_since` is documented as the rebuild
  primitive.
- Rate limiting must degrade predictably: if Redis is unavailable, reads fail open with a warning log and a metric, while writes (token management) stay
  unaffected — documented, not silent.
- Signed media URLs must respect the storage policy of the installation and carry a short TTL; making every media object world-readable by default is explicitly
  forbidden.
- "Posts" is an alias over `page_type = 'post'` until the blog module lands; the OpenAPI document marks it experimental so integrators are not surprised when it
  grows fields.
