# REQ-019 — Headless CMS

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core API (`crates/content` + `apps/api`)
> **Source:** owner brief — platform feature pool (2026-09-25)

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

- [ ] `GET /api/v1/content/pages` with a valid token returns only published pages of the token's site scope, newest first by `updated_at`, with `next_cursor`
  present while more rows exist.
- [ ] Walking the cursor returns each page exactly once across three pages of `limit=2`.
- [ ] `fields=slug,title` returns only those keys plus the always-present identity keys, and an unknown field is refused with `400 invalid_parameter` naming the
  parameter.
- [ ] A request without a token answers `401 invalid_token`; a token for another organization's site answers `404 not_found` (never a cross-tenant leak).
- [ ] A token without `media:read` calling `/api/v1/content/media` answers `403 insufficient_scope`.
- [ ] A token with `expires_at` in the past answers `401 token_expired`, and the Tokens tab shows the row as `expired`.
- [ ] Rotation invalidates the previous secret immediately (old secret → `401`) and returns a new plaintext exactly once.
- [ ] Revoking a token answers `401` on the next call, and the panel row reads `revoked`.
- [ ] The 121st request inside a minute at the Standard tier answers `429` with a `Retry-After` header, and the usage table records one throttled request.
- [ ] `etag` / `updated_since` let a caller fetch only changed items (proven by two sequential calls where only one item changed).
- [ ] `GET /api/v1/content/openapi.json` returns a document that parses as valid JSON, declares `openapi: 3.1.0`, and contains every route in the API table with
  its permission scope.
- [ ] The Explorer executes a real call against the running API, shows status, headers and timing, and its cURL snippet reproduces the same response when pasted
  into a shell.
- [ ] Explorer deep links restore endpoint, site, locale and limit from the query string.
- [ ] The usage tab renders a non-empty chart after the QA walkthrough has made real calls, with per-token rows matching the counts the explorer produced.
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
