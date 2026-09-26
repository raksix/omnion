# REQ-040 — API Gateway

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`apps/api` edge)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Expose to the outside world:

```text
/api/v1
/api/v2
```

while managing:

- API keys
- OAuth
- rate limit
- quotas
- analytics
- versioning
- scopes
- IP restrictions

## Notes

- Complements the versioned-API design in docs/02-ARCHITECTURE.md and the service-account
  identities in docs/07-IAM.md §15.

## Implementation spec

### Scope (in / out)

**In**

- A clear split between the **data plane** (everything under `/api/v1` and `/api/v2`) and the
  **control plane** (`/api/v1/gateway/*`, consumed by the panel — the gateway manages itself through the same API).
- Three identities on one surface: panel session cookie, **API key** (`Authorization: Bearer` with a prefixed, hashed key) and **OAuth2** client-credentials access token.
- **Scopes** drawn from the existing permission catalogue, so a key can never be broader than a role could be, and an endpoint's required permission *is* its scope.
- **Rate limits** (per key, token bucket) and **monthly quotas**, with standard response headers and a distinct error code per limit.
- **IP restrictions**: a per-key CIDR allow-list that is checked after authentication.
- **Versioning lifecycle**: a registry of API versions with `current` · `supported` · `deprecated` · `sunset` states, deprecation and sunset headers on responses, and a compatibility report for the v1 → v2 move.
- **Analytics**: daily aggregates per key plus a bounded, sampled request log for the last N requests, feeding the usage screen.
- A request id assigned at the edge, returned in every response, and propagated into audit rows (REQ-039) and logs.

**Out**

- GraphQL (separate request), gRPC, WebSocket gateway behaviour (REQ-041), mTLS and WAF features.
- Billing or invoicing for API usage; quotas are operational limits, not money.
- An SDK generator (REQ-022 developer portal) — this request only publishes an OpenAPI document.
- Managing third-party APIs; the gateway fronts Omnion and nothing else.

### Screens (UI)

- `/gateway` — overview. Traffic chart (requests per day, 30 days), cards for **Requests today · Error rate · Rate-limited requests · Quota warnings**, a **Top keys** table and a list of keys approaching their monthly quota. Links to each sub-screen.
- `/gateway/keys` — table: **Name · Key id · Owner · Scopes · Rate limit · Quota · Last used · Expires · Status**. Status: `active` `expiring` `revoked`. Filters: status, owner, scope, expiring within 30 days, unused for 90 days, free text. Bulk actions: **Revoke** (typed confirmation) **· Rotate · Extend expiry · Export metadata** (CSV, never the secret).
- Create drawer fields: **Name** (required, ≤ 80) · **Owner** (person or service account) ·
  **Scopes** (multi-select over the catalogue, with a search and a "grant what this role has" helper) · **Rate limit** (requests per minute, 1–100 000, tier presets 60/600/6000) ·
  **Monthly quota** (requests, must be ≥ the per-minute limit) · **IP allow-list** (one CIDR per line, optional) · **Expires** (date, optional) · **Description**. Validation: scopes must exist in the catalogue, CIDR entries must parse, the expiry must be in the future. On success the value is shown **once** in a copy panel with a "I have stored this key" confirmation, and there is no path to retrieve it again.
- `/gateway/keys/{id}` — detail with tabs **Overview · Scopes · Usage · Requests · Rotations**. Rotate explains the grace window (default 24 h) and shows the old key id still valid until then; Revoke is immediate and irreversible.
- `/gateway/clients` — OAuth clients: **Name · Client id · Grant types · Redirect URIs · Scopes · Status · Created**. Add form: Name · Grant types (`client_credentials` `authorization_code`) · Redirect URIs (required for `authorization_code`, must be `https` or a loopback address) · Scopes · Description. The client secret is shown once, like a key.
- `/gateway/policies` — reusable limit policies: **Name · Kind · Applies to · Value · Window · Enabled**. Kinds: `rate_limit` `quota` `ip_restriction`. A policy can be attached to a key, a client or an organization default; conflicts resolve to the most specific binding and the screen says which one is winning.
- `/gateway/usage` — analytics. Table: **Key · Requests · Errors · 4xx · 5xx · p95 · Quota used · Day**. Filters: key, owner, date range, status class. A key drill-down shows the daily series, the top paths and the last sampled requests (time, method, path, status, duration, IP).
- `/gateway/versions` — version registry: **Version · Status · Released · Deprecated · Sunset · Notes** with actions **Publish · Deprecate · Sunset** and a v1 → v2 compatibility report listing changed and removed endpoints.
- States: skeleton rows; "no keys yet" empty state with a create call to action; a revoked key shows as such with the revocation timestamp; a rate-limit error never renders as a blank page; the analytics table shows "no requests in this range" rather than zeros everywhere.
- Keyboard: `/` search, `n` new key, `r` rotate the focused key, `Esc` closes the drawer. Mobile: tables become cards, the create drawer becomes a full-height sheet, the chart scrolls horizontally.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/gateway/overview` | Counters and quota warnings | `gateway.read` |
| GET | `/api/v1/gateway/keys` | List API keys (metadata only) | `gateway.read` |
| POST | `/api/v1/gateway/keys` | Create a key; the value is returned once | `gateway.keys.manage` |
| GET | `/api/v1/gateway/keys/{id}` | Key detail | `gateway.read` |
| PATCH | `/api/v1/gateway/keys/{id}` | Rename, scopes, limits, allow-list, expiry | `gateway.keys.manage` |
| POST | `/api/v1/gateway/keys/{id}/rotate` | Issue a new secret with a grace window | `gateway.keys.manage` |
| POST | `/api/v1/gateway/keys/{id}/revoke` | Revoke immediately | `gateway.keys.manage` |
| DELETE | `/api/v1/gateway/keys/{id}` | Delete a revoked key record | `gateway.keys.manage` |
| GET | `/api/v1/gateway/keys/{id}/usage` | Daily usage for one key | `gateway.read` |
| GET | `/api/v1/gateway/keys/{id}/requests` | Sampled recent requests | `gateway.read` |
| GET | `/api/v1/gateway/clients` | List OAuth clients | `gateway.read` |
| POST | `/api/v1/gateway/clients` | Create a client; secret returned once | `gateway.clients.manage` |
| PATCH | `/api/v1/gateway/clients/{id}` | Edit URIs, grants, scopes | `gateway.clients.manage` |
| POST | `/api/v1/gateway/clients/{id}/rotate-secret` | Rotate the client secret | `gateway.clients.manage` |
| GET | `/api/v1/gateway/policies` | List limit policies | `gateway.read` |
| POST | `/api/v1/gateway/policies` | Create a policy | `gateway.policies.manage` |
| PATCH | `/api/v1/gateway/policies/{id}` | Edit a policy | `gateway.policies.manage` |
| GET | `/api/v1/gateway/usage` | Aggregated usage by key and day | `gateway.read` |
| GET | `/api/v1/gateway/versions` | Version registry and status | `gateway.read` |
| POST | `/api/v1/gateway/versions/{version}/deprecate` | Mark a version deprecated | `gateway.policies.manage` |
| POST | `/api/v1/gateway/versions/{version}/sunset` | Retire a version | `gateway.policies.manage` |
| GET | `/api/v1/openapi.json` | OpenAPI document for the requested version | any authenticated identity |
| POST | `/api/v1/oauth/token` | Client-credentials and refresh grants | client credentials |

Data-plane behaviour, applied in this middleware order: **request id → version resolution (404 `unknown_version` when the path version is retired) → authentication → scope check (403 `insufficient_scope`) → IP restriction (403 `ip_not_allowed`) → rate limit (429 `rate_limited` with `Retry-After`) → quota (429 `quota_exceeded`) → handler → usage meter**. Successful responses carry `X-Request-Id`, `X-RateLimit-Limit`, `X-RateLimit-Remaining` and `X-RateLimit-Reset`; deprecated versions add `Deprecation` and `Sunset` headers. Error bodies are the standard error shape with `code`, `message` and `request_id`. Key values are never logged, never returned after creation, and are compared as hashes.

### Data model

`database/migrations/0015_api_gateway.sql`:

- `api_clients` — `id uuid pk`, `organization_id uuid not null`, `name text not null`, `kind text not null check (kind in ('service_account','oauth_client'))`, `owner_user_id uuid`, `client_id text not null unique`, `secret_hash text not null`, `redirect_uris text[] not null default '{}'`, `grant_types text[] not null default '{}'`, `scopes text[] not null default '{}'`, `status text not null default 'active' check (status in ('active','revoked'))`, `created_by uuid`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`.
- `api_keys` — `id uuid pk`, `organization_id uuid not null`, `client_id uuid references api_clients(id) on delete cascade`, `name text not null`, `key_id text not null unique` (the public prefix), `secret_hash text not null`, `scopes text[] not null default '{}'`, `rate_limit_per_min int not null default 600 check (rate_limit_per_min between 1 and 100000)`, `quota_per_month bigint not null default 1000000`, `ip_allowlist cidr[] not null default '{}'`, `expires_at timestamptz`, `last_used_at timestamptz`, `revoked_at timestamptz`, `revoked_by uuid`, `description text`, `created_by uuid`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`.
- `api_key_versions` — `id uuid pk`, `api_key_id uuid not null references api_keys(id) on delete cascade`, `key_id text not null unique`, `secret_hash text not null`, `status text not null default 'active' check (status in ('active','grace','revoked'))`, `created_at timestamptz not null default now()`, `grace_until timestamptz`, `revoked_at timestamptz`.
- `gateway_policies` — `id uuid pk`, `organization_id uuid not null`, `name text not null`, `kind text not null check (kind in ('rate_limit','quota','ip_restriction'))`, `target_kind text not null check (target_kind in ('key','client','organization'))`, `target_id uuid`, `value jsonb not null default '{}'`, `enabled boolean not null default true`, `created_by uuid`, `created_at timestamptz not null default now()`. Unique `(organization_id, name)`.
- `api_request_log` — `id bigserial pk`, `organization_id uuid`, `key_id uuid`, `request_id uuid not null`, `method text not null`, `path text not null`, `route_version text not null`, `status int not null`, `duration_ms int not null`, `bytes_in int`, `bytes_out int`, `ip inet`, `user_agent text`, `error_code text`, `created_at timestamptz not null default now()`. Retained briefly and sampled; the retention class is registered with REQ-038.
- `api_usage_daily` — `id bigserial pk`, `organization_id uuid`, `key_id uuid not null`, `day date not null`, `requests bigint not null default 0`, `errors bigint not null default 0`, `client_errors bigint not null default 0`, `server_errors bigint not null default 0`, `p95_ms int not null default 0`, `quota_used bigint not null default 0`. Unique `(key_id, day)`.
- `api_versions` — `id uuid pk`, `version text not null unique`, `status text not null check (status in ('current','supported','deprecated','sunset'))`, `released_at timestamptz`, `deprecated_at timestamptz`, `sunset_at timestamptz`, `notes text`.

Indexes: `api_keys_org_status_idx (organization_id, status)`, `api_keys_prefix_idx (key_id)`, `api_key_versions_key_idx (api_key_id, created_at desc)`, `api_request_log_key_created_idx (key_id, created_at desc)`, `api_usage_daily_key_day_idx (key_id, day desc)`. Counters (token bucket, monthly usage) live in Redis with a periodic flush into `api_usage_daily`; PostgreSQL remains the source of truth for metadata.

### Events

- `api_key.created` · `api_key.rotated` · `api_key.revoked`
- `api_key.limit_reached` — `{key_id, window, limit}` (rate limit)
- `api_key.quota_warning` (at 80 %) and `api_key.quota_exceeded`
- `api_client.created` · `api_client.secret_rotated`
- `api.version.deprecated` · `api.version.sunset`

Webhook relevance: quota warnings are the payload an operator actually wants in chat before an integration breaks; empty-key or many-4xx patterns are useful too, but noisy rate-limit events are aggregated per key and window rather than emitted per request. Payloads carry the key id and the limit, never the secret.

### Acceptance criteria

- [ ] A key is created with scopes, limits, an optional allow-list and an expiry; the value is shown once and cannot be retrieved afterwards.
- [ ] The list, detail, filters and bulk actions show metadata only — no response contains a key value or a secret hash.
- [ ] A request bearing a valid key returns the requested resource and stamps `X-Request-Id`.
- [ ] A request with a revoked or unknown key is `401` with `invalid_key`.
- [ ] A key whose scopes lack the endpoint's permission is `403` with `insufficient_scope`.
- [ ] A key with an allow-list refuses a request from an address outside it with `403 ip_not_allowed`.
- [ ] Exceeding the rate limit returns `429 rate_limited` with `X-RateLimit-Remaining: 0` and a `Retry-After` header; the refusal is counted in usage.
- [ ] Exceeding the monthly quota returns `429 quota_exceeded`.
- [ ] Rotation keeps the previous key valid for the grace window and then rejects it.
- [ ] Expiry is enforced: an expired key is `401`.
- [ ] OAuth client-credentials returns a token that works on the data plane and carries the client's scopes only.
- [ ] Revoking a client invalidates its tokens on the next request.
- [ ] Requesting a retired version path returns `404 unknown_version`; a deprecated version responds with `Deprecation` and `Sunset` headers.
- [ ] Usage aggregates appear per key and day after real traffic, and the sampled request list shows the last requests with status and duration.
- [ ] `/gateway/keys` shows unused keys and quota warnings without a click.
- [ ] `gateway.read` alone cannot create, patch, rotate or revoke (`403`).
- [ ] Key values never appear in logs, audit rows or webhook payloads (asserted by a test).
- [ ] `cargo test --workspace` and `pnpm typecheck && pnpm build` pass; the walkthrough covers the new routes with zero high findings.

### QA plan

- The browser walkthrough visits `/gateway`, `/gateway/keys`, a key detail, `/gateway/clients`, `/gateway/policies`, `/gateway/usage` and `/gateway/versions`; the inventory gains those routes.
- Controls to exercise: create a key with scopes and a limit (asserting the once-only value panel and that it disappears after confirmation), filter the list by status and owner, rotate with the grace notice, revoke with the typed confirmation, the usage table and key drill-down, the version registry actions, and the empty state on a fresh installation.
- API-level checks (integration tests, real HTTP): a data-plane call with a live key returns 200; after revoke it returns 401; a rate-limited loop gets 429 with the headers; a quota-exceeded key gets 429 `quota_exceeded`; an out-of-range IP gets 403; a backgrounded request id appears in the audit trail.
- The visual check must see: a traffic chart with real data, status chips with icon plus text, no key value anywhere on the page, readable tables at 1280 px and card layout under 640 px, and no raw error text from the API surfacing as a toast.

### Slices

1. **Keys + auth middleware** — migration, key creation with hashing and one-time display, the bearer-key authentication path, request-id middleware, list and detail screens.
   *Done:* a real key authenticates a data-plane call, a revoked one is `401`, and the walkthrough sees the list and the create drawer.
2. **Scopes, IP restrictions, rate limits, quotas** — scope enforcement from the permission catalogue, the allow-list check, the token bucket in Redis, quota counting, the standard headers, and policy reuse.
   *Done:* each refusal path returns its documented status and code in tests, and usage counters move.
3. **OAuth clients + analytics** — client-credentials flow, client screens, daily aggregation and the sampled request log, `/gateway/usage`.
   *Done:* a token obtained through the flow works on the data plane, and the usage screen shows the traffic it produced.
4. **Versioning lifecycle + policy polish** — the version registry, deprecation/sunset headers, the compatibility report, `/gateway/versions`, permission guards and audit rows on every control-plane mutation.
   *Done:* a deprecated version answers with the headers, a sunset version is refused, and QA covers every route.

### Risks / notes

- Rate-limit and quota counters depend on Redis; when it is unavailable the edge must fail open for availability but record the gap, or fail closed for strict tenants — this is a documented choice, not a silent default.
- Hashing every key on every request is expensive: index by the public prefix, then verify one hash; reject unknown prefixes before hashing.
- Aggregating usage must not slow the data plane down — flush counters asynchronously and accept a sub-minute lag in the panel.
- `api_request_log` is the highest-volume table in the platform: sample, partition by day and register a retention class with REQ-038 from the first release.
- Secret material handling on the client side: the once-only panel is the only chance to store the value; the UI must not offer "show again" and must not put the value in a URL or clipboard log.
- `/api/v2` only means something with a compatibility contract — publish the deprecation policy (announcement window, sunset window) alongside the registry, and keep v1 supported until the report is clean.
- CORS: the data plane is also called from browsers. Allow-list origins per organization, never `*` with credentials, and preflight requests must not consume quota.
