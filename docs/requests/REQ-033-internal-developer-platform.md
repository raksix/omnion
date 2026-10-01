# REQ-033 — Internal Developer Platform

> **Status:** in-progress (`a1840487`, `416a58bf`, `dd00be30`, `0469249e`, `d497b8f9`, `bfd699dd`, `de3b6620`, `ba95b808`, `4867981f`, `6fa5d647`, `2595999e`, `a560b0b7`, `a7e373d2`; tick 111 — **the panel screen tick 110 said was still open is built, and the status line was the last thing to learn it.** `6fa5d647` ships `/developer/oauth-apps` (1418 lines, 45 `data-oauth-app-*` hooks) and `a560b0b7` ships the depth pass that drives all 33 of the hooks it names — the two halves cross-checked here hook-for-hook, **33 demanded / 33 present**, because a hook renamed on either side leaves a pass that silently measures the *previous* screen and reports it green. The pass carries ~30 gated claims including the ones that need a device flow to be worth anything: the one-time secret must not read as a placeholder, Done stays disabled until the acknowledgement is ticked so a stray click cannot discard the only copy, and the last grant box cannot be unticked. `2595999e` adds the contract gate for the same class of bug one layer down: `apps/admin/lib/types.ts` is hand-written, and **a stale type is still a well-formed type** — rename a field in Rust and `tsc` stays green while the screen renders `undefined` as a plausible wrong answer. The gate reads the Rust `pub` field names and requires each in the TypeScript, with the credential rule (no read shape exposes a credential) keyed on **field names with one named carve-out**, because `previous_secret_expires_at` is a deadline and not a secret; keying on the substring `secret` would have flagged the one field that must be shown.
>
> Tick 109's note stands unchanged for the record: slice 3c, the sessionless half, is shipped — the authorization request, the consent screen, the token endpoint and token introspection, in `crates/developer/src/oauth_flow.rs` and `apps/api/src/routes/oauth_flow.rs`, plus migration `0232` for the access tokens. **A live XSS in my own code, found by the consent screen's escaping test**: the three hidden form fields were JSON-encoded, and JSON escapes a quote as `\\"` where HTML wants `&quot;`, so a `state` of `"><script>alert(1)</script>` closed the attribute and left a live script tag in the approver's session on the platform's own origin. The same function also rendered an absent value as the four characters `null`, which the token endpoint would have echoed back to the client as a `state` it never sent. The compiler caught a third one: `redirect_with_error` takes `&'static str`, and I had written `error.to_string()` — a client-supplied `response_type` was about to ride in a `Location` header into browser history and `Referer`. Gates: `omnion-api --lib` **408** (was 397), `omnion-developer --features store --lib` **128** (was 104), `tsc --noEmit` clean, and migration `0232` proved against live PostgreSQL by **6 named refusals across 5 constraints** plus 4 positive controls. **Open: the panel screen** at `/developer/oauth-apps` and its walkthrough route, which is why the browser pass is not owed on this tick.)
>
> **Slice 3's code is complete. What remains for it is the browser pass**, and that pass is not owed on this tick: the global QA slot is held by a live `w3` run (holder pid 2914377, cwd `/mnt/apopic/omnion-w3`), and `/mnt/apopic` is at 100% with 501 MB free. Gates re-run at tick 111 on the merged tree: `omnion-developer --features store --lib` **128 passed**, `omnion-media --lib` **248 passed**, `probe-oauth-contract.cjs` **56/56**, `pnpm typecheck` **2/2**.)
> **Three things tick 109 found that the code did not know about itself.** (The previous tick's
> findings — the `[::1].attacker.example` open redirect, the `0229` migration collision, and the
> two-fields-one-JSON-key response shape — are in `docs/BUILD-LOG.md` and in the slice 3b notes
> below.)
>
> **(1) A live stored XSS in the consent screen, reachable by any developer who can register an
> app in their own tenant.** The three hidden form fields were **JSON-encoded**, and JSON escapes
> a double quote as `\"` where HTML wants `&quot;`. A `state` of `"><script>alert(1)</script>`
> therefore rendered as
>
>     <input ... name="state" value="\"><script>alert(1)</script>">
>
> with the attribute closed early and the script tag live in the **approver's** session on the
> platform's own origin. The attacker is the app's tenant; the victim is the person signing in.
> The general "nothing leaked" assertion is what found it, but it could not say *which* mistake
> had been made, so the regression test is a standalone one that demonstrates the JSON form
> **does** break out of the attribute — the difference between the two escapers is the bug.
>
> **(2) An absent `state` rendered as the four characters `null`.** A hidden input whose value is
> `null` posts the *string* `"null"`, which `usable_state` accepts, and the token endpoint then
> echoes it back to the client as a `state` it never sent. The platform was inventing a value on
> the client's behalf. Absence now renders as an empty attribute.
>
> **(3) The borrow checker caught a third, before a test could.** `redirect_with_error` takes
> `&'static str` for the error code and the description — that signature is the rule, and I had
> written `&error.to_string()`. `UnknownGrantType` carries the caller's own `response_type` in its
> message, so a client-supplied value was about to ride in a `Location` header into browser
> history, proxy logs and the next `Referer`. The fixed `access_denied` goes to the browser; the
> real reason goes to a `tracing::debug`.
>
> **Slice 2 remains code-complete and waiting only on a browser pass.**
>
> **A fourth thing from tick 108, found by the response-shape tests:** `MintedAppResponse` declared
> `previous_secret_expires_at` while the flattened `OAuthApp` already carries a field of that
> name. Two fields, one JSON key, and the flatter writes first — so on a *creation* the response
> said `null` while also claiming to have no such field, and a client reading the outer value
> could never learn a rotation's deadline. The duplicate is gone and a `debug_assert` states that
> the minted app and its row must agree about the deadline.
>
> **Gates:** `cargo test -p omnion-developer --features store --lib` **104** (was 73, +31),
> `cargo test -p omnion-api --lib` **397** (was 391), `cargo test -p omnion-permissions --lib`
> **64**, the OpenAPI drift gate **5/5** with all four new routes documented, and migration
> `0231` proven against live PostgreSQL by **8 named refusals across 7 constraints** —
> `oauth_apps_deletion_is_whole` in *both* directions, plus `oauth_apps_overlap_is_whole`,
> `oauth_apps_grant_types_are_known`, `oauth_apps_redirect_uris_known`,
> `oauth_codes_challenge_is_whole`, `oauth_codes_expiry_is_future` and
> `oauth_apps_org_name_key` — with **3 positive controls**, including the one that matters: a
> *withdrawn* app frees its name, so the partial unique index is doing its job. · **Captured:**
> 2026-09-25 · **Layer:** `apps/admin` + SDKs · **Source:** owner brief — platform periphery &
> headline features (2026-09-25)
> brief — platform periphery & headline features (2026-09-25)

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
- [x] OAuth apps reject non-`https` redirect URIs except `http://localhost`, and an authorization-code plus PKCE flow completes end to end. *(both halves are now built and proved. The **rejection** half was slice 3a/3b: `app_rules::validate_redirect_uris` refuses a non-`https` scheme unless the host is exactly `localhost`, `127.0.0.1` or `[::1]`, compared as whole strings and never as a prefix; the refusal carries the row's **position** and never the URL, because `Display` reaches a log and `Debug` reaches a panic message. The **flow** half is slice 3c: `GET /oauth/authorize` validates the client, the grant, the redirect, the scopes and the PKCE challenge **in that order and writes nothing**; `POST /oauth/consent` mints the code, re-running the whole check against the app *as it is now* (the screen was rendered from a GET, and an app can be withdrawn or narrowed in between); `POST /oauth/token` redeems the code single-use, re-checks that the redirect matches the one the code was issued for, verifies the verifier against the stored challenge by hashing both sides, and only then mints a token scoped to the **consented** set. The code→token exchange is proved in the database by `oauth_codes_challenge_is_whole` and `redeem_code`'s `used_at is null` predicate living *inside* the update — a read-then-update would let two simultaneous redemptions both succeed, and that is invisible in every test that redeems a code once.)*
- [ ] Client secret rotation keeps the previous secret valid until its overlap expiry, then rejects it. *(proved in code and in the database. `rotate_secret` moves the old hash into the overlap slot **by the same expression** that writes the new one — `previous_secret_hash = client_secret_hash` inside the `update` — so there is no window in which the old secret is in neither slot, and migration `0231`'s `oauth_apps_overlap_is_whole` refuses a partial write at the database level. `which_secret_matched` filters the overlap **by the clock before comparing** and returns *which* slot matched: `a_previous_secret_is_honoured_only_inside_its_overlap_and_the_slot_is_reported` asserts the second-before boundary works, the instant of expiry does not, and a window that expired years ago authenticates nobody. The slot name is what lets the audit row distinguish a deployment that has not redeployed from one that has, which is the only reason the overlap exists. The panel half shipped in `6fa5d647` and shows the deadline in the list row and the detail drawer.)*
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
   **Code-complete** (`be241bd2`, `de029671`, `54853888`): the document, the runtime, the routes, the screen and the depth pass all ship; the drift check is a test that runs in `cargo test --workspace`, which is what CI runs. **Open on the browser pass alone.**
3. **OAuth apps + events catalog.** App registration and editing, secret rotation with overlap, authorization-code plus PKCE, catalog from the event registry, webhook deep link.
   **Slices 3a and 3b shipped** (`a1840487`, `416a58bf`, `dd00be30`; tick 108). 3a: the client material, the redirect-URI rule, PKCE and the migration — **renumbered `0229` → `0231`**, because w8 holds `0229_crm_lead_sla_index_terminal_status.sql` and the migration namespace is shared across every worktree (two files numbered 229 make sqlx answer `VersionMismatch(29)` for the whole database). 3b: the store (`crates/developer/src/store_oauth.rs`), the authorization check (`authorize()` in `model_oauth.rs`), the six panel routes, both permission keys, and the seven documented operations.
   **A real open-redirect defect in 3a's own code was found by 3b's tests** (`816b89d5`): `http://[::1].attacker.example/cb` was accepted as loopback, because the fix that stopped `split(':')` reducing `[::1]` to `[` took a bracketed authority whole *as a literal*. The closing bracket is now honoured only when nothing but an optional decimal `:port` follows it.
   **Panel screen shipped** (`6fa5d647`, `2595999e`, `a560b0b7`; tick 110's commits, status corrected at tick 111). `/developer/oauth-apps` — list with name/status/grant filters, detail drawer, register, edit, rotate, suspend/resume, withdraw-with-confirmation, the one-time secret dialog and the empty/populated/loading/error states. The pass and the screen are cross-checked hook-for-hook (33/33) because a rename on either side leaves a pass that measures the previous screen and calls it green. The risk note on the overlap window ("show the expiry in the UI") is honoured in **two** places, the list row and the detail drawer, so an operator who never opens the drawer still sees that a previous secret is live. **Open: the browser pass** — the slot is held by a live `w3` run.
   **Slice 3c shipped** (`0469249e`, `d497b8f9`, `bfd699dd`, `de3b6620`, `ba95b808`, `4867981f`; tick 109). The sessionless half: `crates/developer/src/oauth_flow.rs` (the request parsing, the code, the access token, the scope-narrowing rule, the grant provenance) and `apps/api/src/routes/oauth_flow.rs` (the four endpoints, the consent screen, the RFC 6749 error shapes, the introspection response), plus migration `0232` for `oauth_access_tokens`.
   **Three defects, all in this tick's own code, and the tick is mostly about them.** A **live stored XSS** in the consent screen: the hidden fields were JSON-encoded, and JSON escapes a quote as `\"` where HTML wants `&quot;`, so a `state` of `"><script>…` closed the attribute and left a live script tag in the approver's session on the platform's own origin — reachable by any developer who can register an app in their own tenant, executed in the victim tenant's user's session. An absent value rendered as the four characters `null`, which the consent POST accepts and the token endpoint would echo back as a `state` the client never sent. And the **borrow checker** caught a third before a test could: `redirect_with_error` demands `&'static str`, and I had passed `error.to_string()` — which would have put a caller-supplied `response_type` into a `Location` header, and from there into browser history, proxy logs and the next `Referer`. The signature was the rule; the type error was the compiler enforcing it.
   The event catalogue half of this slice is largely already built by REQ-016 — `/api/v1/events/catalogue` reads the same compiled registry `omnion_events::catalogue` — so what remains there is the developer framing, not a second source of truth.
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
