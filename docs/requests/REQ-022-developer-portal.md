# REQ-022 — Developer Portal

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + core
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Inside Omnion:

```text
Developer

API Keys
Webhooks
OAuth Apps
Plugins
Themes
API Docs
Logs
Sandbox
```

Swagger/OpenAPI documentation can be generated automatically.

## Implementation spec

### Scope (in / out)

**In**

- A `/developer` section of the admin app with the eight brief entries as real screens: overview, API keys, webhooks (reuses REQ-016), OAuth apps, plugins, themes, API docs (generated OpenAPI), logs, sandbox.
- **API keys:** create (name, scopes, optional expiry), reveal the secret **once**, list with prefix/last-used/expiry, rotate (old secret dead immediately), revoke, per-key usage and request log.
- **OAuth apps:** register (name, description, icon, homepage, redirect URIs, scopes), client id public / secret hashed, rotate secret, archive, per-user authorization records with revoke.
- **API docs:** the OpenAPI document is generated from route annotations at build time and served at `/api/v1/openapi.json`; the admin embeds an explorer (tag nav, operation list, schema panes, `Copy as cURL`) whose "Try it" sends a real request with a selected key.
- **Logs:** request log filtered by key/app, method, path prefix, status class and time window, with per-request detail (status, duration, matched permission — no request body).
- **Sandbox:** a request console aimed at a non-production base URL with a sandbox-scoped key, and a banner that is unmistakable when the target is production.
- **Plugins / Themes:** read-only portal views that reuse the permissions and endpoints of REQ-044 and REQ-062 and link into their own screens — the portal does not re-implement install flows.
- Every credential action writes an audit row; a key or secret value never appears in a log, audit row, event payload or webhook body.

**Out**

- Third-party developer signup, external accounts, billing, plan-based rate tiers (a global per-key limit exists; tiers do not).
- SDK generation, client libraries, Postman export. Marketplace publishing of API apps (REQ-048).

### Screens (UI)

| Route | Purpose |
|---|---|
| `/developer` | Overview: key count, requests today, error rate, recent failures, quick links |
| `/developer/api-keys`, `/developer/api-keys/{id}` | Key list; create/rotate/revoke; detail with scopes, usage chart, request log |
| `/developer/webhooks` | Embedded endpoint list linking to the REQ-016 screen |
| `/developer/oauth-apps`, `/{id}` | App list + create/edit dialog; detail with redirect URIs, scopes, authorizations, rotation history |
| `/developer/plugins`, `/developer/themes` | Installed packages and themes (read-only) with links to their own screens |
| `/developer/docs` | Generated OpenAPI explorer |
| `/developer/logs` | Request log table, filters, detail drawer |
| `/developer/sandbox` | Request console with key picker and environment banner |

- Keys table columns: Name · Prefix (`omn_live_7f3a…`) · Scopes (count, expandable) · Created · Last used · Expires · Status (active/expired/revoked) · actions.
- Create-key dialog: Name (3–64, required, unique per organization) · Scopes (multi-select from the catalogue grouped by category, at least one) · Expiry (never / 30 / 90 / 365 days / custom date not in the past) · Environment (live/sandbox). Client-side validation is re-checked server side; a duplicate name in the same environment is rejected.
- One-time reveal: full key, `Copy`, a warning that it cannot be shown again, and an explicit "I have stored it" acknowledgement before closing.
- OAuth app form: Name (required) · Description (≤ 400) · Homepage URL (absolute https) · Redirect URIs (1–10, absolute https, no fragments; plain-http loopback allowed only when the deployment enables local development mode) · Scopes (non-empty) · Icon (media picker).
- Logs columns: Time · Method · Path · Status · Duration · Key · Actor; filters by key, method, path prefix, status class, window; `Reset` clears; CSV export of the current view.
- Sandbox layout: rail = tag/operation tree from the spec, middle = parameter form generated from the operation schema, right = response viewer (status, duration, headers). A red banner appears when the target is not the sandbox base URL.
- States: per-screen empty states (`No API keys yet — create your first`), loading skeletons, error state with request id and retry; a failed key creation keeps the form values.
- Keyboard: `/` focus search, `n` new key from the list, `Enter` submit, `Esc` close, `⌘Enter` send in the sandbox, `g` then `d` jumps to `/developer`.
- Mobile: tables collapse to cards, the docs explorer becomes a stacked operation picker, the sandbox form is one column, dialogs become full-height sheets.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/developer/overview` | Counters + recent failures for the overview cards | `developer.read` |
| GET | `/api/v1/developer/api-keys` | List keys (name, prefix, scopes, last used, status) | `developer.keys.read` |
| POST | `/api/v1/developer/api-keys` | Create a key; returns the secret exactly once | `developer.keys.manage` |
| GET | `/api/v1/developer/api-keys/{id}` | Key detail + usage window | `developer.keys.read` |
| POST | `/api/v1/developer/api-keys/{id}/rotate` | Rotate: new secret, previous one invalid at once | `developer.keys.manage` |
| DELETE | `/api/v1/developer/api-keys/{id}` | Revoke (soft: `revoked_at`) | `developer.keys.manage` |
| GET | `/api/v1/developer/oauth-apps` | List OAuth apps | `developer.oauth.read` |
| POST | `/api/v1/developer/oauth-apps` | Create an app; returns the client secret once | `developer.oauth.manage` |
| PATCH | `/api/v1/developer/oauth-apps/{id}` | Update metadata, redirect URIs, scopes | `developer.oauth.manage` |
| POST | `/api/v1/developer/oauth-apps/{id}/rotate-secret` | Rotate the client secret | `developer.oauth.manage` |
| DELETE | `/api/v1/developer/oauth-apps/{id}` | Archive the app and revoke its authorizations | `developer.oauth.manage` |
| GET | `/api/v1/developer/oauth-apps/{id}/authorizations` | Users who granted access | `developer.oauth.read` |
| GET | `/api/v1/developer/logs`, `/logs/{id}` | Request log with filters; one request incl. matched permission | `developer.logs.read` |
| GET | `/api/v1/developer/usage` | Requests/day and error rate per key | `developer.usage.read` |
| GET | `/api/v1/developer/scopes` | Assignable scope catalogue, grouped by category | `developer.read` |
| GET | `/api/v1/openapi.json` | Generated OpenAPI document | public (rate-limited) |
| POST | `/api/v1/developer/oauth/token` | Client-credentials token for a key/app pair | public (client auth) |
| POST | `/api/v1/developer/oauth/authorize` | Consent decision for a user | `developer.oauth.manage` |

Errors: `400` validation, `401` bad/expired credential, `403` permission miss, `404` unknown id, `409` duplicate name, `422` unusable scope combination, `429` rate limit. A revoked or expired key gets `401` with a stable error code, never a stack trace and never the key echoed back.

### Data model

Migration: `database/migrations/0012_developer_portal.sql` (next free number at build time).

- `api_keys` — `id uuid pk`, `organization_id uuid not null references organizations(id) on delete cascade`, `name text not null`, `environment text not null default 'live'`, `key_prefix text not null`, `key_hash text not null`, `scopes text[] not null`, `created_by uuid null references users(id) on delete set null`, `last_used_at timestamptz null`, `expires_at timestamptz null`, `revoked_at timestamptz null`, `rotated_from uuid null`, `created_at timestamptz not null default now()`; checks `environment in (live,sandbox)`, `cardinality(scopes) between 1 and 64`, `length(btrim(name)) between 3 and 64`; indexes unique `(key_hash)` (the lookup path), unique `(organization_id, lower(name), environment) where revoked_at is null`, `(organization_id, created_at desc)`. Only the hash is stored; plaintext exists once in the create/rotate response.
- `api_key_usage_daily` — `api_key_id uuid references api_keys(id) on delete cascade`, `day date`, `requests int not null default 0`, `errors int not null default 0`, `avg_duration_ms int not null default 0`; primary key `(api_key_id, day)`.
- `oauth_apps` — `id uuid pk`, `organization_id uuid not null`, `name text not null`, `description text not null default ''`, `client_id text not null unique`, `client_secret_hash text not null`, `redirect_uris text[] not null`, `scopes text[] not null`, `homepage_url text null`, `icon_media_id uuid null references media(id) on delete set null`, `created_by uuid null`, `archived_at timestamptz null`, `last_secret_rotated_at timestamptz null`, `created_at timestamptz`, `updated_at timestamptz`; checks `cardinality(redirect_uris) between 1 and 10` with an absolute-https pattern (loopback http only under the deployment flag) and `cardinality(scopes) between 1 and 64`; unique `(organization_id, lower(name)) where archived_at is null`.
- `oauth_authorizations` — `id uuid pk`, `app_id uuid not null references oauth_apps(id) on delete cascade`, `user_id uuid not null references users(id) on delete cascade`, `scopes text[] not null`, `granted_at timestamptz not null default now()`, `revoked_at timestamptz null`; unique `(app_id, user_id)`, index `(user_id)`.
- `api_request_logs` — `id bigint generated always as identity pk`, `organization_id uuid null`, `api_key_id uuid null references api_keys(id) on delete set null`, `actor_user_id uuid null`, `method text not null`, `path text not null`, `status smallint not null`, `duration_ms int not null`, `permission text null`, `client_fingerprint text null` (a keyed hash, never a raw address), `created_at timestamptz not null default now()`; indexes `(organization_id, created_at desc)`, `(api_key_id, created_at desc)`, `(status, created_at desc)`; pruned by a scheduled job whose retention window is shown on the logs screen.
- Usage rollup and request log are written by middleware in `apps/api`, so a new route is covered without extra code.

### Events

- **Emitted:** `developer.api_key.created`, `developer.api_key.rotated`, `developer.api_key.revoked`, `developer.oauth_app.created`, `developer.oauth_app.secret_rotated`, `developer.oauth_app.authorized`, `developer.oauth_app.revoked`.
- **Consumed:** none required for correctness; `user.deleted` anonymises authorizations, and a permission-catalogue change refreshes the scope picker (a vanished scope is removed from new keys and flagged on existing ones).
- Webhook relevance: credential-lifecycle events are exactly what a security team subscribes to — an organization can wire an endpoint to `developer.*` and get a signed delivery on every rotation. Payloads carry ids, name, environment and scope names; never a secret or a hash.

### Acceptance criteria

- [ ] `/developer` appears in the sidebar with all eight entries and each loads a real screen.
- [ ] Creating a key shows the secret once; the list shows only a prefix afterwards.
- [ ] A key authenticates on a guarded endpoint and is rejected after revocation.
- [ ] Expiry is enforced (`401` past `expires_at`) and the UI labels the key `expired`.
- [ ] Rotation invalidates the previous secret immediately and keeps usage history.
- [ ] Duplicate key names in the same environment are rejected with a readable message.
- [ ] Scope picker lists the catalogue grouped by category; a key without a scope gets `403` on that route.
- [ ] OAuth app creation returns the client secret once; redirect URIs are validated (absolute, ≤ 10).
- [ ] Secret rotation leaves old tokens dead and records the rotation time.
- [ ] Authorizations list shows granting users and can revoke one.
- [ ] `/api/v1/openapi.json` returns a valid document covering every mounted route.
- [ ] The explorer renders operations by tag and `Copy as cURL` produces a runnable command.
- [ ] Sandbox `Send` performs a real request showing status, duration and the environment banner.
- [ ] Logs filter by key, method, status class and window; detail shows the matched permission.
- [ ] No secret, key value or raw client address appears in any log, event or audit row.
- [ ] A user without `developer.*` sees no Developer nav group and gets `403` from the endpoints.
- [ ] Plugins and Themes screens show real installed items and link into their own screens.
- [ ] Keyboard and mobile behaviour match the spec, including the one-time secret panel.
- [ ] `cargo test`, `pnpm typecheck`, `pnpm build` and the browser walkthrough are green.

### QA plan

The walkthrough must: open `/developer`, click all eight entries and assert each renders content (no dead links); create a key with two scopes, copy the one-time secret, reload and confirm only the prefix remains; call a guarded endpoint with that secret (`200`), revoke it (`401`); create an OAuth app with two redirect URIs and rotate the secret; run one `GET` operation from the sandbox and read the response pane; filter logs by status class and open one request detail.

Visual check should see: aligned key-table columns with a monospace prefix, a one-time reveal panel that cannot be mistaken for a permanent value, code panes with no clipped JSON, correct active/expired/revoked badges, and an environment banner that is unmistakable in production.

### Slices

1. **Keys + logs backend.** Migration, key create/list/rotate/revoke, request-log middleware, usage rollup, `developer.*` catalogue permissions, `apps/api/src/routes/developer.rs`. *Done when:* a key created through the API authenticates a guarded call and its request appears in the log with the matched permission.
2. **Portal screens.** `/developer` overview, keys list with one-time reveal + rotate/revoke, logs list + detail, nav entry, all three states, keyboard and mobile behaviour. *Done when:* the walkthrough creates, uses and revokes a key entirely from the UI.
3. **Docs + sandbox.** Build-time OpenAPI generation, `/api/v1/openapi.json`, embedded explorer with `Copy as cURL`, sandbox console and environment banner. *Done when:* an operation executed from the sandbox returns a real response and the document validates as OpenAPI.
4. **OAuth apps + package views.** OAuth registration, secret rotation, authorizations, archives, read-only plugin/theme views with links. *Done when:* an authorization can be granted and revoked and both read-only views show real rows.

### Risks / notes

- Secret hygiene is the point of this REQ: hash at rest, reveal once, never log. Copying a secret into a log line, event payload or audit detail is a defect, not a nit.
- The explorer sends real requests from the browser; it must use the API host with the caller's cookie or selected key and must never persist a key value in local storage.
- Reusing REQ-044/REQ-062 endpoints keeps one install path; if those REQs land later, the portal views ship read-only over what exists and hide install controls rather than faking them.
- Request-log retention trades disk for usefulness: prune on a schedule, publish the window in the UI, and never let the log table replace the audit trail (`crates/audit` is that).
