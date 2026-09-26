# REQ-033 — Internal Developer Platform

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + SDKs
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Inside Omnion:

```text
Developer
├── API Explorer
├── API Keys
├── OAuth Apps
├── Webhooks
├── Events
├── Logs
├── Plugin SDK
├── Theme SDK
├── Workflow SDK
└── CLI
```

A developer can extend the system without leaving it.

## Notes

- Extends the Developer Portal (REQ-022).

## Implementation spec

### Scope (in / out)

In:

- A Developer section in the admin covering all ten surfaces: API Explorer, API Keys, OAuth Apps, Webhooks, Events, Logs, Plugin SDK, Theme SDK, Workflow SDK, CLI.
- API key lifecycle: create with scopes and environment (`live` / `sandbox`), rotate, revoke, expiry, last-used, per-key usage history and rate tier.
- OAuth apps: client registration (redirect URIs, scopes, branding), authorization-code plus PKCE for organization-internal clients, secret rotation with an overlap window, app status.
- API Explorer driven by the OpenAPI document the platform serves itself: browse operations, fill parameters and body from a schema-generated form, send against the selected environment, inspect status/latency/body, and copy the call as curl, TypeScript or Python.
- Events catalog: every published event type with description, JSON Schema payload, and a validating sample, with a deep link that prefills a webhook subscription.
- Request logs: filterable API request history with per-request detail (sizes and metadata only, no bodies) and CSV export.
- SDK scaffolds: generate a plugin, theme or workflow starter from a template, preview the file tree, validate a manifest, and download the archive.
- CLI: install instructions and a device-code login that mints a scoped CLI token.

Out:

- Marketplace publishing, review and payouts (REQ-023 / REQ-048).
- API usage metering and billing.
- Hosting third-party (non-organization) OAuth clients and public consent screens.
- A general-purpose HTTP proxy: the Explorer calls only this platform's own API, with the caller's own credentials.
- Plugin execution and sandboxing internals — this REQ ships tooling, not the runtime model.

### Screens (UI)

Routes (`apps/admin/app/developer/*`, feature dir `apps/admin/features/developer/`):

```text
/developer                     ← overview
/developer/api-explorer
/developer/keys
/developer/keys/{id}
/developer/oauth-apps
/developer/oauth-apps/{id}
/developer/webhooks            ← REQ-016 surface, framed here
/developer/webhooks/{id}
/developer/events
/developer/logs
/developer/sdks                ← tabs: Plugin | Theme | Workflow | CLI
```

- Shared layout: left sub-nav with the ten entries, an environment badge (`Live` / `Sandbox`) pinned in the header, and a quickstart card on the overview with three copy-ready snippets.
- `/developer` overview cards: Active keys, OAuth apps, Webhook endpoints (with 24h failure rate), Requests 24h (with error rate), Recent events — each linking to its surface. Quickstart tabs (curl / TypeScript / Python) use a placeholder token, never a real one.
- Keys table columns: Name, Prefix, Environment, Scopes (chips with `+N` overflow), Created, Last used, Expires, Status, and a row menu (Rotate, Revoke, View logs). Filters: environment, status, scope, name. Bulk: revoke selected (typed confirmation above five) and copy prefixes.
- Key create form: Name (required, 3–60 chars), Environment (required radio), Scopes (grouped multi-select, at least one, with a `select all read` shortcut), Expiry (never / 30 / 90 / 365 days, default 90), IP allowlist (optional CIDR list, validated), Rate tier (standard / high; `high` requires an owner or admin role). Submission opens a one-time secret dialog with a copy button, a not-shown-again warning, and a `Generate another key` action.
- Key detail: daily usage chart (requests/errors), top paths table, and the latest 20 requests with a link into Logs pre-filtered by that key.
- OAuth apps table: Name, Client ID (copyable), Redirect URIs (count), Scopes, Status, Created. Create/edit form: Name, Description, Logo (png/svg ≤256 KB), Redirect URIs (one per line; `https` required except `http://localhost`), Allowed scopes, Grant types (authorization code + PKCE, optional client credentials). Secret rotation shows the new secret once and explains the overlap window.
- Events catalog: left list of event names grouped by domain with search; right pane shows description, collapsible JSON Schema tree, a validating sample payload, and `Subscribe a webhook`, which deep-links to the webhook form with the event preselected.
- Logs table columns: Time, Method, Path, Status, Duration, Key, Actor, Request ID. Filters: key, status class, path prefix, method, date range (default 24h), duration threshold. A row opens a drawer with the request summary (sizes, timing, region, request id) and a copy-as-curl action; the filtered view exports to CSV through the REQ-031 export machinery.
- Log drawer must state plainly that bodies are not stored, so the absence of payload data is understood rather than suspected.
- SDK tab: template picker (Plugin — TypeScript, Theme — TypeScript, Workflow — DSL project), slug-validated Name, Target (Live / Sandbox), and a file-tree preview of the archive before download. A `Validate manifest` drop zone reports schema errors inline with line numbers.
- CLI tab: per-platform install snippet, `omnion login` device-code flow with a code, the approval URL and an expiry countdown, plus a plain-language list of the scopes the issued token will carry. Existing tokens are never rendered.
- Empty states: keys — "Henüz API anahtarı yok" with the create CTA; logs — "Bu aralıkta istek yok" with a widen-range action; events — suggestion chips; SDK — template cards only. Loading: skeleton tables, and a cancellable sending state in the Explorer. Errors: field-level inline messages; a failed Explorer call renders as a normal result (status, body, latency), not a page error; a `403` names the missing permission, never the caller's roles.
- Keyboard: `Ctrl+K` reaches every surface (REQ-032); `e` or `Cmd+Enter` sends in the Explorer, `Cmd+/` toggles the snippet drawer, `g k` keys, `g l` logs, `g e` events, `?` shortcut sheet.
- Mobile: sub-nav becomes a select, tables become cards, the Explorer stacks (request then response) with a sticky `Send`, copy buttons are ≥44px targets, and long snippets scroll horizontally instead of wrapping mid-token.

### API

| Method | Path | Purpose | Permission |
| --- | --- | --- | --- |
| GET | `/api/v1/dev/openapi.json` | OpenAPI document for the caller's surface | `developer.read` |
| POST | `/api/v1/dev/explorer/requests` | Run one API call as the caller (no key material involved) | `developer.explorer.run` |
| GET | `/api/v1/api-keys` | List keys (metadata only) | `developer.keys.read` |
| POST | `/api/v1/api-keys` | Create a key; returns the secret exactly once | `developer.keys.manage` |
| POST | `/api/v1/api-keys/{id}/rotate` | Rotate; returns the new secret once | `developer.keys.manage` |
| DELETE | `/api/v1/api-keys/{id}` | Revoke | `developer.keys.manage` |
| GET | `/api/v1/api-keys/{id}/usage` | Daily request/error series | `developer.keys.read` |
| GET | `/api/v1/oauth-apps` | List apps | `developer.oauth.read` |
| POST | `/api/v1/oauth-apps` | Register an app; returns the client secret once | `developer.oauth.manage` |
| PATCH | `/api/v1/oauth-apps/{id}` | Edit metadata, redirect URIs, scopes | `developer.oauth.manage` |
| POST | `/api/v1/oauth-apps/{id}/secret/rotate` | Rotate the client secret with an overlap window | `developer.oauth.manage` |
| DELETE | `/api/v1/oauth-apps/{id}` | Delete an app and revoke its tokens | `developer.oauth.manage` |
| GET | `/api/v1/events/catalog` | Event types with schema and sample | `developer.events.read` |
| GET | `/api/v1/request-logs` | Paged request log with filters | `developer.logs.read` |
| GET | `/api/v1/request-logs/{id}` | Single request metadata (no bodies) | `developer.logs.read` |
| POST | `/api/v1/dev/sdks/scaffold` | Generate a starter archive | `developer.sdks.scaffold` |
| POST | `/api/v1/dev/manifests/validate` | Validate a plugin/theme/workflow manifest | `developer.sdks.scaffold` |
| POST | `/api/v1/dev/cli/device-code` | Start the CLI login device-code flow | `developer.read` |
| POST | `/api/v1/dev/cli/device-code/approve` | Approve a device code from the browser session | `developer.keys.manage` |

Webhook endpoints, deliveries and replay stay on the REQ-016 surface (`/api/v1/webhooks`, `/api/v1/webhooks/{id}/deliveries`, `/api/v1/webhooks/deliveries/{id}/replay`) and are framed under `/developer/webhooks` rather than duplicated.

### Data model

Migration `database/migrations/0013_developer_platform.sql`.

`api_keys`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `uuid pk` | |
| `organization_id` | `uuid not null` | fk `organizations` |
| `name` | `text not null` | unique per organization |
| `prefix` | `text not null` | public identifier, unique |
| `secret_hash` | `text not null` | one-way hash; plaintext never persists |
| `scopes` | `jsonb not null` | array of permission keys |
| `environment` | `text not null` | check in (`live`,`sandbox`) |
| `rate_tier` | `text not null default 'standard'` | check in (`standard`,`high`) |
| `ip_allowlist` | `jsonb` | CIDR array; null means any |
| `expires_at` | `timestamptz` | null means no expiry |
| `last_used_at` | `timestamptz` | |
| `revoked_at` | `timestamptz` | |
| `created_by` | `uuid not null` | fk `users` |
| `created_at` | `timestamptz not null default now()` | |

Indexes: unique `(organization_id, name)`, unique `(prefix)`, `(organization_id, revoked_at)`.

`api_key_usage_daily`: `api_key_id uuid not null` fk `api_keys`, `day date`, `requests integer not null default 0`, `errors integer not null default 0`, `p95_ms integer`, primary key `(api_key_id, day)`.

`oauth_apps`: `id uuid pk`, `organization_id uuid not null`, `name text not null`, `description text`, `logo_object_key text`, `client_id text not null unique`, `client_secret_hash text not null`, `previous_secret_hash text`, `previous_secret_expires_at timestamptz`, `redirect_uris jsonb not null`, `scopes jsonb not null`, `grant_types jsonb not null`, `status text not null default 'active'` check in (`active`,`suspended`,`deleted`), `created_by uuid not null`, `created_at`, `updated_at`. Index `(organization_id, status)`.

`oauth_authorization_codes`: `code_hash text pk`, `app_id uuid not null` fk `oauth_apps`, `user_id uuid not null`, `redirect_uri text not null`, `scopes jsonb not null`, `code_challenge text`, `expires_at timestamptz not null`, `used_at timestamptz`. Index `(app_id, expires_at)` for the sweeper.

`api_request_logs`: `id bigserial pk`, `organization_id uuid not null`, `api_key_id uuid`, `actor_user_id uuid`, `method text not null`, `path text not null`, `status smallint not null`, `duration_ms integer not null`, `request_id text not null`, `bytes_in integer`, `bytes_out integer`, `error_code text`, `created_at timestamptz not null default now()`. Indexes: `(organization_id, created_at desc)`, `(api_key_id, created_at desc)`, `(organization_id, status, created_at desc)`. Retention 14 days by dropping old partitions; bodies are never stored.

`sdk_scaffolds`: `id uuid pk`, `organization_id uuid not null`, `kind text not null` check in (`plugin`,`theme`,`workflow`), `name text not null`, `target text not null`, `object_key text not null`, `byte_size bigint`, `created_by uuid not null`, `created_at` — an audit of generations, not a code store.

### Events

Emitted: `api_key.created`, `api_key.rotated`, `api_key.revoked`, `oauth_app.created`, `oauth_app.secret_rotated`, `sdk.scaffold.generated`. Payloads carry ids, names, scopes and the actor — never key material of any kind.

Consumed: `webhook.delivery.failed` (REQ-016) to surface endpoint health on the overview card; the event catalog is read from the bus registry at request time so it cannot drift.

Webhook relevance: yes — the key and app lifecycle events are exactly what a security-conscious organization subscribes to (alert on key creation or rotation), and the catalog itself documents every subscribable type for the same subscribers.

Audit: key create/rotate/revoke, OAuth app create/edit/delete and secret rotation, and scaffold generation. Explorer calls that mutate are audited on the owning endpoint; read-only Explorer calls appear only in `api_request_logs`.

### Acceptance criteria

- [ ] Creating a key returns the secret exactly once; no later request or page reload returns it again.
- [ ] `secret_hash` is one-way and no endpoint response, log line or rendered page contains plaintext key material.
- [ ] Revoked and expired keys receive `401` with a reason that does not echo the key.
- [ ] Scopes are enforced: a key holding only read scopes cannot perform a write request.
- [ ] IP allowlist entries reject requests from outside the listed CIDRs.
- [ ] Rotating a key issues a new secret once, invalidates the old secret immediately, and the UI states that behaviour.
- [ ] The API Explorer lists operations from the served OpenAPI document, and a CI check fails when the document drifts from the running router.
- [ ] Explorer sends run as the signed-in caller; a call the caller could not make from the UI returns the same `403`.
- [ ] The Explorer shows status, duration and body, and copies the request as curl, TypeScript and Python with a placeholder instead of a real secret.
- [ ] OAuth apps reject non-`https` redirect URIs except `http://localhost`, and an authorization-code plus PKCE flow completes end to end.
- [ ] Client secret rotation keeps the previous secret valid until its overlap expiry, then rejects it.
- [ ] The Events catalog lists only event types the caller may subscribe to, and every sample validates against its own schema.
- [ ] Request logs filter by key, status class, path prefix and date range, and history stays readable after a key is revoked.
- [ ] A log entry contains no bodies and no secret-looking values (asserted against the redaction list in tests).
- [ ] Plugin, theme and workflow scaffolds generate archives that install or load from a clean checkout.
- [ ] Manifest validation reports schema errors with line numbers and rejects invalid manifests.
- [ ] The CLI device-code flow issues a scoped token, and approving it from an account without `developer.keys.manage` is refused.
- [ ] Every mutating developer action appears in the audit log with actor, target and scopes.

### QA plan

Browser walkthrough:

1. `/developer` overview renders the cards and quickstart; snippets copy to the clipboard and contain no secret value.
2. Create a key (Live, read scopes, 90-day expiry) → the one-time dialog appears; copy the secret; reload → the secret is absent from the DOM and the table shows prefix plus expiry.
3. Call the API with the new key from a terminal → `200` on a read endpoint and `403` on a write endpoint (proves scope enforcement).
4. `/developer/api-explorer` → open the customers list operation → `Send` → real data returns; `Cmd+/` opens the snippet drawer; run the copied curl outside the browser with the placeholder replaced → same result.
5. `/developer/events` → pick `customer.created` → the sample validates; `Subscribe a webhook` prefills the subscription form.
6. Register an OAuth app with two redirect URIs (one plain `http` non-localhost, one `http://localhost`) → the invalid one is rejected inline; complete an auth-code plus PKCE flow with a test client.
7. `/developer/logs` → filter by the new key → the step 3 and step 4 calls are present with correct statuses and durations; open the detail drawer → confirm no bodies are shown and the explanation is visible.
8. `/developer/sdks` → generate a plugin scaffold → the archive downloads and the file-tree preview matches; drop a broken manifest → inline schema errors with line numbers.
9. CLI tab → start the device-code flow → approve it from a second session → the CLI receives a token and can call `/api/v1/me` within the granted scopes.
10. Sign in as a read-only developer role → management controls are absent and a direct `POST /api/v1/api-keys` returns `403`.
11. Keyboard and mobile: `g k` reaches keys; at 390×844 the sub-nav is a select, tables are cards, and the Explorer stacks.

Visual check: the one-time secret dialog is unmistakable (warning icon, explicit not-shown-again copy, copy button with feedback); status columns use icon plus label rather than colour alone; the Explorer's request/response split is legible at 1280px; code blocks scroll instead of breaking layout; the environment badge stays pinned.

### Slices

1. **Keys + logs.** Migration, key CRUD with rotate/revoke, secret hashing, request-log middleware with filters, `/developer/keys` and `/developer/logs`.
   Done: a key created in the UI authenticates a real call, is scope-enforced, and appears in the logs with the correct status and duration.
2. **API Explorer.** OpenAPI emission, operation browser, schema-driven request form, send-as-caller, snippet drawer, CI drift check.
   Done: Explorer steps from the walkthrough pass and the drift check runs in the pipeline.
3. **OAuth apps + events catalog.** App registration and editing, secret rotation with overlap, authorization-code plus PKCE, catalog from the event registry, webhook deep link.
   Done: a local test client completes the flow and every catalog sample validates against its schema.
4. **SDKs + CLI + polish.** Scaffold generator, manifest validator, CLI device-code, overview cards, permission-hidden controls, mobile layout.
   Done: scaffolds install or load, `omnion login` issues a scoped token, and the read-only role sees no management controls.

### Risks / notes

- Secret handling is the headline risk: hash at rest, display once, never in URLs, logs, telemetry or error text; keep a test that greps every response during the walkthrough for the secret value.
- OpenAPI drift would teach wrong calls: emit the document from the same router definition and fail the pipeline when a route lacks annotations.
- The Explorer can resemble a privileged proxy: it must run with the caller's session and permissions only, and it is rate-limited per user.
- Overlap windows briefly double the valid-secret surface: cap the window, log old-versus-new secret usage distinctly, and show the expiry in the UI.
- Request logs are attractive to attackers: store no bodies, keep the retention window explicit (14 days), and mask client identifiers according to the compliance policy.
- Manifest validation must use the same code path as the runtime loader; a laxer validator produces extensions that pass review and fail to boot.
- Device-code phishing: codes are short-lived, bound to the approving user, displayed with requesting-client metadata, and cannot be approved by a session lacking key-management permission.
- Scaffolds get copied into public repositories: ship a README warning against committing tokens and a placeholder-only example environment file.
