# REQ-022 — Developer Portal

> **Status:** in-progress — **slices 1 and 2 shipped.** Slice 1 (`0240_developer_portal.sql`, `omnion-developer`, the routes, the `developer.*` catalogue, 8 walks) is in the BUILD-LOG for tick 108. **Slice 2 is the request-log middleware plus the four portal screens**, and it closed a defect that had been invisible for a whole slice: `logs_store::record` existed, was walked, and **nothing on the request path ever called it**, so the request log was empty in a platform whose whole purpose here is a debugging surface. Two walks and a static gate this tick; the browser pass is still open and that is why this REQ is not `done`. · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + core
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
      — **The nav half of this is closed and walked-free on purpose**: the sidebar asks
        `GET /developer/scopes` once (`lib/developer-access.tsx`) and hides the group from an
        account that may not be there, treating a failure as "no". The three entries that exist
        are the three screens slice 2 ships; the other five are slices 3 and 4, and a nav link to
        a screen that does not exist is exactly the dead control the definition of done forbids.
        **Still open:** the browser pass has not visited the group, and the criterion says *eight*.
- [x] Creating a key shows the secret once; the list shows only a prefix afterwards.
      — The guarantee is a property of the response **types**, not of anybody's memory: `IssuedKey`
        is the only shape in `omnion-developer` with a `token` field, and every other read answers
        `KeyView`, which has no such field. `no_response_carries_a_secret_a_second_time` walks the
        real JSON of the list, the detail and the log screen and greps it for the token, **the stored
        SHA-256 hash**, and any field named `secret` — a check that greps only for the token passes on
        a response that echoes the hash, which is just as fatal.
      — **The panel half, tick 109:** `scripts/qa/probe-developer-wiring.cjs` reads
        `lib/developer.ts` and asserts that exactly one exported shape carries a `token: string`
        and that `DeveloperKey`'s field list has none. The reveal dialog also refuses to close
        before "I have stored this" is ticked, so a stray `Esc` cannot destroy a secret nobody
        has written down, and a refused clipboard degrades to showing the value rather than to
        losing it.
- [x] A key authenticates on a guarded endpoint and is rejected after revocation.
      — `a_key_authenticates_a_guarded_call_and_dies_the_moment_it_is_revoked`, over the real router
        against a real database. The key answers `200` on `/developer/sandbox/probe` (guarded by
        `require_or_developer_key`), its `last_used_at` is then read **out of PostgreSQL**, and after
        `DELETE` the *identical token bytes* are refused `401`. The row survives the revoke, because a
        request made five minutes earlier must still name the key that made it.
- [x] Expiry is enforced (`401` past `expires_at`) and the UI labels the key `expired`.
      — `an_expiry_past_dies_the_key_and_the_list_says_so`. The criterion has three halves and the walk
        asserts all of them in order, because the cheapest version of this test passes against a key
        that never authenticated: **the key works first** (`200` on the sandbox probe with an expiry
        30 days out), **then the identical bytes stop** once the instant passes, **then the panel
        says so** — `status: "expired"` in the list, the row under `?status=expired`, the row *absent*
        from `?status=active`, and the overview counting it under `expired` and not under `active`.
        The filter half is separate from the label on purpose: a list that labels correctly but filters
        on the wrong column hides the key under "Active", which is the confusing case rather than the
        obvious one.
        The instant is reached by **writing the column**, not through the API: `create_key` refuses an
        expiry in the past at mint time, which is the right product decision and makes the negative
        case unreachable from outside. `expires_at <= now()`, not `<` — a key whose second has
        arrived must not still be live, and an off-by-one there is invisible until somebody sets a
        one-second expiry.
        Writing this walk also corrected an assumption in the walk beside it: `list_keys` answers a
        **bare array**, not `{ "keys": [...] }`. The assertion named the contract it expected and the
        contract turned out to be a different one, which is the useful outcome — a walk accepting
        either shape would have proved nothing about which one the server serves.
- [x] Rotation invalidates the previous secret immediately and keeps usage history.
      — `rotation_kills_the_previous_secret_immediately_and_keeps_the_old_row`. Rotation **inserts a
        successor** with `rotated_from` set and revokes the predecessor in one transaction, rather than
        overwriting `key_hash`. Overwriting would have been three lines shorter and would have destroyed
        the half of this box that says *keeps usage history*: the predecessor's rollup and its log rows
        keep its id, and an audit row pointing at a deleted id names nothing. The walk asserts the old
        secret is refused, the new one answers `200`, the predecessor row still exists with a
        `revoked_at`, and the successor's `rotated_from` points back at it.
- [x] Duplicate key names in the same environment are rejected with a readable message.
      — `409 duplicate_name`, and the message names **both** the name and the environment. The check is
        in the store *and* in the partial unique index, because a constraint violation answers `23505`
        and nothing else, and the panel needs a sentence. The index is partial (`where revoked_at is
        null`) so a revoked key's name is freed: an operator who revokes and re-creates with the same
        name is doing exactly what they mean to do, and without the partial index the only workaround
        would be a suffix. `the_migration_applies_to_a_populated_database` asserts the `409` *after*
        re-applying the migration, because an index that only exists on a fresh database is not an
        index.
- [x] Scope picker lists the catalogue grouped by category; a key without a scope gets `403` on that route.
      — Two walks, because the criterion is really two claims. The **picker**:
        `GET /developer/scopes` groups `omnion_permissions::catalogue::CATALOGUE` in Rust rather than
        in the panel — a panel that re-derived the grouping would drift the day a category is renamed —
        and each row carries `grantable`, so the form cannot offer something the server will refuse.
        The **403**: `a_key_without_the_routes_scope_is_refused_with_a_named_gap` gives the key a real,
        catalogued scope (`analytics.read`) that is simply not the one the route needs, and the refusal
        names *the scope to add* rather than the route. The complementary rule — a key may only delegate
        what its issuer holds — is `a_key_cannot_be_minted_with_a_scope_its_issuer_does_not_hold`, which
        grants `developer.keys.manage` **without** `iam.users.manage` and asserts both the `400` and that
        no row was stored. Without that combination, holding the manage key would be a pass to
        everything.
- [ ] OAuth app creation returns the client secret once; redirect URIs are validated (absolute, ≤ 10).
- [ ] Secret rotation leaves old tokens dead and records the rotation time.
- [ ] Authorizations list shows granting users and can revoke one.
- [ ] `/api/v1/openapi.json` returns a valid document covering every mounted route.
- [ ] The explorer renders operations by tag and `Copy as cURL` produces a runnable command.
- [ ] Sandbox `Send` performs a real request showing status, duration and the environment banner.
- [ ] Logs filter by key, method, status class and window; detail shows the matched permission.
      — **The backend and the screen halves are closed; the browser pass is not.** The four
        filters are wired to the four query parameters the store already honoured, `Reset` clears
        them together, and the drawer prints the scope the guard resolved — the column that
        `check_kind_reporting` was split out to provide. What tick 109's new walk
        `a_request_writes_its_own_log_row_and_nothing_else_does` proves is the half that was
        missing underneath all of it: a session row, a key row and a **403 row** are written
        because the platform served the request, and each carries its permission. Before this
        tick the table was empty.
- [x] No secret, key value or raw client address appears in any log, event or audit row.
      — Three separate leaks, closed at three different places, and none of them was closed by
        remembering to be careful in a handler:
        * **The query string.** `?token=…` is ordinary API practice, so the stored path is stripped by
          `path_without_query` **inside `logs_store::record`** — the last point at which the raw path
          exists. The walk recorded `?access_token=super-secret-value` verbatim until this was added,
          because the caller passed the whole URI. Stripping at the boundary is the only placement a
          future caller cannot forget.
        * **The request body.** There is no column for one.
        * **The client address.** `ClientIdentity::fingerprint` is a **keyed** HMAC over address and
          user-agent, length-prefixed (without that, `1.2.3.4`+`5.6.7.8` and `1.2.3.45`+`6.7.8` hash the
          same bytes — a collision anyone can construct for free). It **refuses** when
          `OMNION_LOG_PEPPER` is unset rather than falling back to an unkeyed hash, because an unkeyed
          digest of an address is a rainbow table away from the address list and the log's own retention
          window guarantees somebody still holds the export in a year.
- [x] A user without `developer.*` sees no Developer nav group and gets `403` from the endpoints.
      — **Backend half closed** by `every_developer_route_is_guarded`: a real organization member holding
        none of the six keys is refused `403 permission_denied` on all eight routes, and anonymously
        `401`. The walk signs in *without* the developer keys on purpose — the family is deliberately not
        in the base role, so that account is the realistic default user, and a suite that signs in as an
        account holding everything is how this REQ's predecessors shipped a guard that was only ever
        satisfied. The **nav half is now built** (tick 109): `lib/developer-access.tsx` asks
        `GET /developer/scopes` once and the sidebar hides the group from an account that may not be
        there, treating any failure as "no". What it still needs is a browser that has *seen* it.
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
