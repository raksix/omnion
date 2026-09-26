# REQ-015 — Integration Hub

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/integrations`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Instead of writing one-off plugins for each service, one integration hub:

```text
Integrations

Google
Microsoft
GitHub
Slack
Discord
Stripe
PayPal
S3
Cloudflare
OpenAI
LDAP
SMTP
```

A provider abstraction lets the user pick their provider for each capability.

## Implementation spec

New module `modules/integrations` (capability abstraction, adapter registry, connection store, delivery log) plus the admin section `/integrations`. The hub translates a **capability** into a concrete provider, so the core asks for `mail.send` instead of naming a vendor. It uses `crates/storage`, `crates/events`, `crates/audit`, `crates/permissions` and the Redis client for retry scheduling. Credential values live behind a reference (secret store when present; otherwise encrypted write-only storage) and are never returned by the API.

### Scope (in / out)

**In**

- Capability model with these keys: `mail.send`, `mail.inbound`, `chat.notify`, `payment.checkout`, `payment.events`, `storage.object`, `identity.directory`, `identity.oauth`, `ai.chat`, `dns.records`, `repository.sync`, `code.hosting.webhook`.
- Adapter registry seeded from code at boot, each adapter declaring its capabilities, a config schema (for form generation and server-side validation), a version and a health check.
- Adapters shipping in this request (each fully working before it appears in the catalogue): SMTP (`mail.send`), S3-compatible object storage (`storage.object`), Slack-compatible incoming webhook (`chat.notify`), OpenAI-compatible chat (`ai.chat`), generic OAuth/OIDC client (`identity.oauth`), generic HTTP webhook sender (`code.hosting.webhook`). The remaining brief providers are added by later slices through the same registry — the catalogue endpoint never lists an adapter that does not work, and the panel never shows a disabled "coming soon" card.
- Connections: named instances per adapter, scoped platform-wide, per organization or per site, with a connection test, enable/disable, edit and disconnect (typed confirmation).
- Capability routing: which connection answers a capability, with a fallback connection and a clear warning when switching would affect live traffic. Resolution is cached in Redis and invalidated on change.
- Outbound dispatch through the hub: callers ask the hub for a capability; the hub resolves the connection, redacts fields, signs where the adapter requires it, records an attempt and retries with backoff up to a configured maximum.
- Inbound provider webhooks: a per-connection secret path (`/api/v1/integrations/{id}/webhook/{token}`), signature verification where the adapter supports it, idempotent recording by external event id, and mapping to a platform event when the adapter declares one.
- Delivery log with payload previews redacted by rule (secret-shaped fields, tokens, email bodies are summarised), retry of failed deliveries, and export.
- Usage counters per connection per capability per day (attempts, failures, average duration).

**Out**

- A generic two-way sync engine with conflict resolution; per-module mapping UIs beyond what each adapter needs; publishing integrations to a marketplace; automating OAuth app registration with the provider; billing metering; provider-side cost reporting; AI tool calling (that stays in the AI Hub and consumes `ai.chat` through this hub).

### Screens (UI)

- `/integrations` — catalogue grid with search and a capability filter. Card: adapter name, version, capability chips, connection state badge (`not connected | connected | error`), last used, and one primary action (`Connect` / `Manage`). Cards exist only for adapters that ship working; the count of available adapters is real.
- `/integrations/{slug}` — connect/manage form generated from the adapter's config schema: text, URL (https required except loopback), number, select, multi-select, switch and write-only secret fields. Secret fields show `Replace` and `Clear` affordances and never render a stored value; the form shows when the value was last set. Sections: connection, scopes/toggles per capability, throttling (max attempts 1–10, backoff seconds). Actions: "Test connection" (inline result with latency and message), "Save", "Disable", "Disconnect" (typed confirmation of the connection name). Validation: required fields, URL shape, numeric ranges, and a schema error rendered against the offending field.
- `/integrations/capabilities` — routing table: **Capability · Provider · Connection · Scope · Fallback · Updated by · Updated at**. Edit drawer to switch provider or connection, with an inline warning when the capability is currently in use; an unresolved capability shows `unassigned` in a warning tone and a link to a matching adapter.
- `/integrations/logs` — delivery table: **When · Connection · Direction · Capability · Target · Status · Attempts · Duration · Error**. Filters: connection, direction, capability, status, date range, free text on target. Row drawer: attempt timeline, redacted request/response previews, and the related platform event when one was emitted. Bulk: retry failed, export CSV.
- `/integrations/usage` — counters table grouped by connection and capability over a selectable range (24 h / 7 d / 30 d): **Connection · Capability · Attempts · Failures · Failure rate · Avg duration**, with a sparkline of attempts.
- `/integrations/settings` — defaults: default scope for new connections, retry policy, log retention days (1–90), inbound webhook base URL (read-only, with a copy button), redaction rules list (field patterns masked in logs), and whether failure notifications go to the notification centre.
- States: skeleton cards while loading; empty states per screen (no connections yet, no deliveries in range, no capability routed); error banner with retry; a connection in `error` state shows its last error on the card. Keyboard: `n` new connection, `t` test connection, `/` filter, `⌘K` palette. Mobile: one card per row, the routing table becomes labelled cards, forms single-column with sticky save.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/integration-adapters` | Shipped adapter catalogue + config schemas | `integrations.read` |
| GET | `/api/v1/integrations` | Connections (paged, filtered) | `integrations.read` |
| POST | `/api/v1/integrations` | Create a connection | `integrations.manage` |
| GET | `/api/v1/integrations/{id}` | Connection detail (secrets masked) | `integrations.read` |
| PUT | `/api/v1/integrations/{id}` | Update config, credentials, throttling | `integrations.manage` |
| DELETE | `/api/v1/integrations/{id}` | Disconnect and delete | `integrations.manage` |
| POST | `/api/v1/integrations/{id}/test` | Run the adapter health check | `integrations.manage` |
| POST | `/api/v1/integrations/{id}/enable` | Enable a connection | `integrations.manage` |
| POST | `/api/v1/integrations/{id}/disable` | Disable a connection | `integrations.manage` |
| GET | `/api/v1/integration-capabilities` | Capability routing table | `integrations.read` |
| PUT | `/api/v1/integration-capabilities/{capability}` | Route a capability to a connection | `integrations.manage` |
| GET | `/api/v1/integrations/{id}/deliveries` | Delivery log for one connection | `integrations.read` |
| POST | `/api/v1/integration-deliveries/{id}/retry` | Retry a failed delivery | `integrations.manage` |
| GET | `/api/v1/integrations/{id}/usage` | Usage counters for a range | `integrations.read` |
| GET | `/api/v1/integration-settings` | Defaults, retention, redaction rules | `integrations.read` |
| PUT | `/api/v1/integration-settings` | Save defaults | `integrations.manage` |
| POST | `/api/v1/integrations/{id}/webhook/{token}` | Inbound provider webhook (signature-verified) | none (signed) |

New catalogue keys (category `integrations`): `integrations.read`, `integrations.manage`. The inbound webhook path carries no session permission by design; it is guarded by the per-connection token plus adapter signature verification, and it is exempt from CSRF because it is not cookie-authenticated.

### Data model

**`integration_adapters`** — seeded from the code registry on boot so foreign keys resolve. `key text pk` (`smtp`, `s3_object`, `slack_incoming`, `openai_chat`, `oauth_client`, `http_webhook`), `name text not null`, `version text not null`, `capabilities text[] not null`, `config_schema jsonb not null`, `created_at timestamptz not null default now()`.

**`integrations`** — `id uuid pk default gen_random_uuid()`, `adapter_key text not null → integration_adapters(key)`, `organization_id uuid null → organizations(id) on delete cascade`, `site_id uuid null → sites(id) on delete cascade`, `scope text not null` (`platform|organization|site`), `name text not null` (1–64), `config jsonb not null default '{}'` (non-secret values only), `credential_ref text null` (secret store reference), `credential_ciphertext bytea null` (write-only fallback when no secret store is configured), `webhook_token text not null default encode(gen_random_bytes(24), 'hex')`, `state text not null default 'connected'` (`connected|disabled|error`), `last_error text null`, `last_tested_at timestamptz null`, `last_used_at timestamptz null`, `max_attempts int not null default 5`, `backoff_seconds int not null default 30`, `created_by uuid null → users(id) on delete set null`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`. Constraints: `scope = 'organization'` requires `organization_id` and forbids `site_id`; `scope = 'site'` requires both; `max_attempts between 1 and 10`; `backoff_seconds between 5 and 3600`. Unique index `(adapter_key, coalesce(organization_id, zero_uuid), coalesce(site_id, zero_uuid), lower(name))`, plus an index on `(state)`.

**`capability_routes`** — `capability text pk`, `integration_id uuid null → integrations(id) on delete set null`, `fallback_integration_id uuid null → integrations(id) on delete set null`, `scope text not null default 'platform'`, `updated_by uuid null → users(id)`, `updated_at timestamptz not null default now()`.

**`integration_deliveries`** — `id bigint generated always as identity pk`, `integration_id uuid not null → integrations(id) on delete cascade`, `direction text not null` (`outbound|inbound`), `capability text not null`, `target text not null default ''`, `status text not null default 'pending'` (`pending|delivered|failed`), `attempts int not null default 0`, `max_attempts int not null default 5`, `next_attempt_at timestamptz not null default now()`, `request_preview jsonb not null default '{}'` (redacted), `response_status int null`, `response_excerpt text null`, `error text null`, `created_at timestamptz not null default now()`, `delivered_at timestamptz null`.

**`integration_events`** — inbound provider events for idempotency and mapping. `id bigint generated always as identity pk`, `integration_id uuid not null → integrations(id) on delete cascade`, `event_type text not null`, `external_id text not null`, `signature_ok boolean not null`, `payload_preview jsonb not null default '{}'` (redacted), `platform_event text null`, `received_at timestamptz not null default now()`. Unique `(integration_id, external_id)`.

**`integration_usage_daily`** — `integration_id uuid not null → integrations(id) on delete cascade`, `capability text not null`, `day date not null`, `attempts int not null default 0`, `failures int not null default 0`, `total_duration_ms bigint not null default 0`, primary key `(integration_id, capability, day)`.

Indexes: `integration_deliveries (integration_id, created_at desc)`; `integration_deliveries (next_attempt_at) where status = 'pending'`; `integration_events (received_at desc)`. Migration: `database/migrations/0015_integration_hub.sql`, append-only, commented like `0009`.

### Events

**Emitted:** `integration.connected`, `integration.disconnected`, `integration.tested`, `integration.delivery.failed`, `integration.delivery.succeeded` (sampled), `integration.capability.routed`, `integration.event.received`. Inbound provider events that an adapter maps become platform events with the adapter's namespace (for example a payment adapter emits `payment.succeeded`); the original is always kept in `integration_events` for replay.
**Consumed:** `page.published` (chat notify example), `user.created` (invitation mail through `mail.send`), `security.finding.opened` (alert routing), `backup.failed` (ops notification), `health.service.degraded` (alert via the `chat.notify` capability).

Webhook relevance: `integration.delivery.failed` and `integration.disconnected` are the two an operations endpoint subscribes to; every payload carries identifiers and the capability, never a credential and never a full provider payload.

### Acceptance criteria

- [ ] `modules/integrations` exists with the capability model, adapter registry and one working adapter per shipped provider.
- [ ] `database/migrations/0015_integration_hub.sql` applies on fresh and populated databases.
- [ ] `/integrations` lists only shipped adapters, each with a real state and a working primary action.
- [ ] Creating an SMTP connection and testing it reports a real result against the configured server.
- [ ] A wrong SMTP password produces a failing test with the provider message and no stored secret echo.
- [ ] Creating an S3-compatible connection and testing it succeeds against the configured bucket.
- [ ] Routing `mail.send` to the SMTP connection makes `user.created` deliver an invitation.
- [ ] Routing `chat.notify` to the webhook adapter delivers a message to a test endpoint.
- [ ] Routing `ai.chat` to the OpenAI-compatible connection answers a chat request through the hub.
- [ ] Switching a capability to a fallback connection takes effect without a restart.
- [ ] A delivery failure retries with backoff, stops at `max_attempts` and lands `failed` with the error.
- [ ] Retry from the log requeues only failed rows and updates the attempt count.
- [ ] Inbound webhook with a valid token and signature is stored once; replaying it creates no duplicate.
- [ ] Inbound webhook with a bad signature is refused with `401` and recorded as `signature_ok = false`.
- [ ] Log previews mask secret-shaped fields; no token or password appears in HTML, JSON or CSV export.
- [ ] Usage counters match the deliveries recorded for the same day and respect the selected range.
- [ ] Scoped connections are visible and manageable only inside their organization/site (scope rule).
- [ ] Every endpoint enforces its catalogue key; the inbound route is reachable without a session.
- [ ] Empty, loading and error states exist on every screen and the routing table shows `unassigned` honestly.
- [ ] Walkthrough passes with zero high findings.

### QA plan

The walkthrough must visit `/integrations`, one adapter connect form, `/integrations/capabilities`, `/integrations/logs`, `/integrations/usage` and `/integrations/settings`, and click: search, the capability filter, connect (with an intentionally wrong value to capture the failure path, then a correct one), test connection, each routing edit, a log row drawer, retry, export CSV, disconnect (confirmation), and mobile cards. Visual check should see: secret fields rendered as empty masked inputs with a `Replace` affordance (never a value), state badges consistent with other centres, redacted previews showing mask characters rather than blank rows, no raw JSON in the drawer, and the mobile pass where each card shows name + state + primary action without horizontal scroll.

### Slices

1. **Capability core + SMTP** — module scaffold, adapter registry and schema validation, connections table, capability routing, `mail.send` through SMTP, catalogue and connect form, test action. Done: an invitation email is delivered through the hub and the panel shows the connection and its test.
2. **Storage + chat + AI adapters** — S3-compatible storage, Slack-compatible incoming webhook, OpenAI-compatible chat adapters, capability routing UI, consumption of `page.published` and `user.created`. Done: `storage.object`, `chat.notify` and `ai.chat` each answer through a routed connection, switchable without a restart.
3. **Deliveries + logs + usage** — delivery table, retry worker with backoff, redaction rules, log screen with drawer and export, usage counters. Done: a forced failure retries to exhaustion, lands `failed`, and a manual retry succeeds; previews show masked fields.
4. **Inbound + OAuth + hardening** — inbound webhook path with token and signature verification, idempotent event store, mapped platform events, OAuth client adapter, disconnect confirmations, failure webhook. Done: a signed inbound event is recorded once, mapped to a platform event, and a replayed copy is ignored.

### Risks / notes

- Credentials are the whole risk: write-only fields, encrypted at rest, referenced never inlined, masked in logs and export, and never in an event or audit payload. A single leak is a release blocker.
- Adapters must be honest: an adapter that cannot pass its own health check must not appear as connected, and a capability with no routed connection must fail loudly to the caller with a clear error instead of silently dropping the action.
- The provider list in the brief is bigger than one request: adding the remaining providers is a registry entry plus an adapter, and the spec deliberately ships fewer, working integrations instead of many stubs.
- Inbound endpoints are internet-facing: rate limit them, verify signatures before parsing payloads, cap body size, and keep the token path out of logs.
- Retry storms against a provider can get the deployment throttled or blocked: per-connection concurrency and backoff are mandatory, and the log must show the attempt timeline.
- Scope leakage is a real defect class here: a site-scoped connection must never be readable from another site or organization, and the resolution cache must be keyed by scope as well as capability.
