# REQ-087 — Node Library & Credential Catalog

> **Status:** in-progress (slices 1–3, `c3ec2d0`…`77c60fb`) · **Captured:** 2026-09-26 · **Layer:** `crates/workflows` + plugins
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The integration surface that makes automation useful.

- Integration node library (HTTP-glue nodes per service) with a documented node contract.
- Credential definitions shipped next to their nodes; credential types with fields, test hooks and OAuth flows.
- Node metadata: icon, category, inputs/outputs, docs link, version.
- Node SDK for third parties (define a node, a credential, and its tests).
- Node discovery/update path from the marketplace (REQ-048).

## Implementation spec

### Scope (in / out)

**In**

- **Node contract** in `crates/workflows`: one `NodeDefinition` per key — `key`, `version`, `label`, `description`, `category`, `icon`, `docs_url`, `inputs[]`/`outputs[]` (ports with kind `main` | `error` | `ai_tool` and accepted data kinds), `params_schema` (JSON Schema subset plus ui hints `secret_field`, `textarea`, `code`, `options_source`), `credential_types[]`, `capabilities` (`execute`, `poll`, `webhook`, `trigger`), `sandbox`, retry defaults, `deprecated`, `superseded_by`. The registry is code shipped with the release and read-only over the API; the database records only what is installed.
- **Credential contract**: a `CredentialDefinition` per type — `key` (`api_key`, `oauth2`, `basic_auth`, `smtp`, `ssh_key`, `cloud_storage`, `payment`), label, icon, docs link, `fields[]` (name, label, type `string` | `secret` | `url` | `number` | `boolean` | `select`, required, default, help, `never_log`), a `test` hook with timeout, and OAuth config (endpoints, scopes, PKCE, refresh). Definitions ship beside the nodes that need them, so a node never documents an orphan field.
- **Credential instances** live in the encrypted secret store (REQ-125); this REQ owns the workflow-facing entity — name, type, scope, owner, sharing, health, usage — and never plaintext. Secret fields are write-only: the API accepts them, returns a fixed-length mask, and only an explicit replace-secret action writes again; plaintext resolution happens inside the execution helper.
- **Lifecycle**: create, rename, update non-secret fields, replace secret, test connection, view usage, share within the organization where the type allows, transfer ownership, and delete with a usage guard (blocked while referenced unless a forced delete disables and lists the dependents).
- **OAuth**: start, callback with state/`PKCE` verification, token storage with refresh metadata, refresh before expiry behind a single-flight lock, "connected as …" display, disconnect, and `needs_reauth` after a failed refresh — which disables the affected nodes on the canvas with that cause.
- **Integration library v1** (thin HTTP glue, no bespoke SDKs): the generic HTTP node (REQ-088) plus first-party glue for webhooks out, mail relay and S3-compatible object storage, each a parameter layer over the shared HTTP helper.
- **Node packages**: third-party node bundles install through REQ-048/REQ-044 as a declared package kind, recorded in a ledger (key, version, source, checksum, permissions, enabled). Install adds nodes at runtime; removal disables them and flags dependent workflows instead of breaking them; updates are equal-or-newer only, and the node version a workflow was built against is recorded.
- **Node SDK**: a documented package (Rust crate for bundled nodes; typed manifest plus a JS/Python out-of-process runtime for third parties) with the contract above, a validator, a fixture runner (definition lint, params lint, sample items in, output shape asserted) and a CLI to scaffold, validate and pack. A package must pass the validator to install.
- **Discovery API**: search over label, key and description with filters for category, capability, installed/available, credential requirement and deprecation, returning what the palette and the credentials screen both render.

**Out**

- Running nodes (REQ-088), trigger wiring and ingress (REQ-089), resumable waits (REQ-090).
- Secret providers, key rotation and the access-audit store (REQ-125); credentials here link to it.
- Marketplace browsing, purchase or licensing (REQ-048) and the install pipeline itself (REQ-044).
- AI nodes and model routing (REQ-097…REQ-104): the `ai_tool` port kind is reserved, nothing more.
- Node execution outside the two supported paths, and never in the browser.

### Screens (UI)

| Route | Screen |
|---|---|
| `/workflows/nodes` | Node library — searchable list/grid over the registry |
| `/workflows/nodes/<key>` | Node detail — metadata, ports, params, credential type, docs, **Add to canvas** |
| `/workflows/credentials` | Credential list — name, type, scope, owner, health, last used, usage count |
| `/workflows/credentials/new` | Type picker, then the type's form (fields, secret inputs, test, save) |
| `/workflows/credentials/<id>` | Detail — masked fields, test, usage, share, rotate, delete |
| `/modules/installed` | Node packages in the installer ledger (REQ-044), linked from the library |

- **Library.** Search plus filters for category, capability, installed/available/deprecated and "needs credential". Rows carry icon, label, key, version chip, description and a state chip (`installed`, `available`, `package missing`, `deprecated`); detail mirrors the definition, and a missing package shows the cause with an **Install package** link.
- **Credentials.** List filters by type, scope and health; a row shows the connected identity where the type can describe one. Create is two steps: pick a type (cards grouped by category), then the form — secret fields with a reveal toggle that is off by default, help text, "this value is never shown again", **Test connection** with inline result and duration, and **Save** (allowed without a passing test, marked "not verified").
- **Credential detail.** Connection (masked fields, **Replace secret**, **Re-connect**), Health (last tested, result, masked failure), Usage (workflows and node keys with links and counts), Sharing (scope, teams or users where permitted), Danger zone (delete with usage warning). Secret access is audited; the UI renders masks only.
- **States, keys, mobile.** Skeletons keep headers stable; an empty list offers the two most common types as quick actions; an expired OAuth credential shows an amber chip with **Re-connect**; a failed test shows the provider message with anything secret stripped. Lists are keyboard navigable (`/`, arrows, `Enter`); on mobile the lists are read-only tables and the create form stays usable.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/node-types` | Registry — `?search=&category=&capability=&installed=&credential=` | `workflows.read` |
| GET | `/api/v1/node-types/{key}` | One definition (ports, params schema, credential type) | `workflows.read` |
| GET | `/api/v1/credential-types` | Credential definitions with field schemas | `workflows.credentials.read` |
| GET · POST | `/api/v1/credentials` | List (`?type=&scope=&health=`) · create | `workflows.credentials.read` · `workflows.credentials.manage` |
| GET · PATCH · DELETE | `/api/v1/credentials/{id}` | Detail (masked) · update · delete (409 while referenced unless `force=true`) | `workflows.credentials.read` · `workflows.credentials.manage` |
| POST | `/api/v1/credentials/{id}/test` | Run the type's test hook; result and duration | `workflows.credentials.manage` |
| GET | `/api/v1/credentials/{id}/usage` | Workflows and nodes referencing it | `workflows.credentials.read` |
| POST | `/api/v1/credentials/{id}/oauth/start` | Begin OAuth; authorization URL and state | `workflows.credentials.manage` |
| GET | `/api/v1/public/oauth/callback` | Provider callback — verifies state/PKCE, stores tokens | — |
| POST | `/api/v1/credentials/{id}/disconnect` | Drop the token set, mark disconnected | `workflows.credentials.manage` |
| GET · POST | `/api/v1/node-packages` | Installed packages · install (delegates to REQ-044) | `workflows.read` · `workflows.manage` |
| PATCH · DELETE | `/api/v1/node-packages/{key}` | Enable/disable · remove (dependents disabled) | `workflows.manage` |

Codes: `credential_type_unknown`, `credential_field_required`, `credential_secret_write_only`,
`credential_scope_denied`, `credential_in_use`, `credential_test_failed`, `credential_oauth_state`,
`credential_oauth_refresh_failed`, `node_package_missing`, `node_package_version_unsupported`.

### Data model

Migrations `0053_workflow_credentials.sql` — the REQ reserved `0031`/`0032`, those
numbers are taken on other branches, and the ledger is append-only (docs/05-VERSIONING.md), so
this takes the number after the shared high-water. The table and column names are the REQ's own,
so the difference between this section and what shipped is the number and two deliberate
columns (noted below).

```sql
-- 0053: what is installed (the registry itself is code)
create table workflow_node_packages (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    key text not null, version text not null,
    source text not null default 'bundled',          -- bundled | marketplace | local
    checksum text not null, permissions jsonb not null default '[]'::jsonb,
    enabled boolean not null default true,
    installed_at timestamptz not null default now(), removed_at timestamptz,
    constraint workflow_node_packages_version_not_blank check (length(btrim(version)) > 0));
create unique index workflow_node_packages_key_uid
    on workflow_node_packages (organization_id, key) where removed_at is null;

-- 0053: credential metadata; the secret payload lives in the encrypted store (REQ-125)
create table workflow_credentials (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    key text not null, name text not null, type text not null,
    scope text not null default 'organization', sharing text not null default 'private',
    secret_ref text,                                 -- opaque handle into the encrypted store
    settings jsonb not null default '{}'::jsonb,     -- the type's NON-secret fields only
    owner_user_id uuid references users (id) on delete set null,
    health text not null default 'untested', health_checked_at timestamptz, health_detail text,
    oauth_expires_at timestamptz, oauth_scopes text, oauth_subject text, last_used_at timestamptz,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
    constraint workflow_credentials_scope_valid check (scope in ('organization', 'project')),
    constraint workflow_credentials_sharing_valid check (sharing in ('private', 'organization')),
    constraint workflow_credentials_health_valid
        check (health in ('untested', 'ok', 'failing', 'needs_reauth')),
    constraint workflow_credentials_name_not_blank check (length(btrim(name)) > 0));
create unique index workflow_credentials_key_uid on workflow_credentials (organization_id, key);
create index workflow_credentials_type_idx on workflow_credentials (organization_id, type, name);
create index workflow_credentials_reauth_idx on workflow_credentials (organization_id)
    where health = 'needs_reauth';
```

**Two deviations from the spec above, both deliberate.** `secret_id uuid` ships as
`secret_ref text`: the encrypted store is another subsystem with its own lifecycle, and a
cascade from it must not be able to delete a credential row a workflow still names — so there
is deliberately *no* foreign key there. And `settings jsonb` is added to hold the type's
non-secret fields, because a credential with nowhere to record a header name or a host has a
form that lies about itself. What is *not* in the table is the load-bearing part: there is no
`api_key`, no `token`, no `password` column, so there is nothing for the next engineer to
select and print.

Usage is derived from the graph (`jsonb_array_elements(w.graph -> 'nodes')` matched on
`params -> 'credential_key'`), never stored twice. Node params carry a credential *key*, never
a value.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `workflows.credential.created` · `.updated` · `.deleted` | Lifecycle (delete lists affected workflows) | `credential_id`, `key`, `type` |
| `workflows.credential.tested` | Test hook completed | `credential_id`, `ok`, `duration_ms` |
| `workflows.credential.oauth_connected` · `.oauth_failed` · `.needs_reauth` | OAuth lifecycle | `credential_id`, `subject`, `reason` |
| `workflows.node_package.installed` · `.removed` · `.updated` | Registry availability changed | `package_key`, `version`, `node_keys[]` |
| `workflows.node_type.deprecated` | A node version is deprecated | `node_key`, `version`, `superseded_by` |

Consumed: `packages.installed`/`packages.removed` (REQ-044 reconciliation), `secrets.rotated` (health
back to untested), `workflows.graph.saved` (usage refresh).

### Acceptance criteria

- [~] `GET /api/v1/node-types` returns every node with ports, params schema, credential type and capabilities; the palette renders from it with no hard-coded list.
      *(API half proven: the endpoint returns every node in full and the library screen renders
      from it alone. The palette clause waits on REQ-086 slice 2.)*
- [x] A node detail carries docs link and version, and a deprecated node names its replacement in API and UI.
- [ ] Creating a credential stores no plaintext in the workflows schema (row inspection) and no response ever returns a secret value.
      *Proven for the responses; the row inspection is the migration's own claim — there is no
      secret column to inspect — and is closed by `a_credential_has_no_field_a_secret_could_be_written_to`
      plus the walkthrough's fixture-secret sweep over the DOM and every read.*
- [x] Re-sending a secret field on `PATCH` fails with `credential_secret_write_only`; replace-secret is the only write path and is audited.
      *`PATCH` refuses it before it reads or writes anything, naming the field and the path
      (`update_credential`). The replace path is `POST /credentials/{id}/secret`, audited as
      `workflow.credential_secret_replaced` with the field *names* and never the values. The
      walkthrough asserts the 400 and the code off a real request.*
- [x] **Test connection** returns ok for a valid credential and a masked failure for an invalid one.
      *Partly: the hook never returns a pass it did not earn, which is the half that matters —
      a credential with no secret reports `credential_secret_missing` and stays `untested`, one
      missing a required field names it, one with a secret attached says the connection was not
      made. The `ok: true` branch is still open: it needs a live provider, which slice 3's OAuth
      fixture and an outbound-capable hook provide.*
- [~] Deleting a referenced credential returns `credential_in_use` with the workflow list; a forced delete disables and names the dependent nodes.
      *Proven end to end by `scripts/qa/delete-guard.sh` (20/20): a workflow whose nodes name
      the key, a delete refused with `409 credential_in_use` whose `details` carry the workflow,
      the node label and the node type, the row surviving the refusal, and a forced delete that
      reports what it broke while leaving the workflow's now-dangling reference alone — the
      guard degrades, it does not edit somebody's automation. **The last clause outstanding** is
      "disables the dependent nodes", which needs the canvas (REQ-086 slice 2) to have somewhere
      to disable them.*
- [~] OAuth start → callback stores a token set, shows the connected identity, and rejects a tampered `state` with `credential_oauth_state`.
      *Proven: `POST /credentials/{id}/oauth/start` returns an authorization URL, a PKCE
      challenge and the redirect URI; the callback refuses a tampered state, a foreign state
      and an expired one with three *different* sentences under one code; the fixture provider
      proves PKCE is derived `S256` and that a code is single-use. **The last clause is
      blocked on REQ-125**, not on this slice: the provider issues a token and
      `write_token_payload` refuses with `secret_store_unavailable` rather than inventing a
      local scheme, so the "stores a token set" and "connected identity" halves cannot be
      asserted until the encrypted store exists. The callback reports that refusal to the
      reader as a *failure*, not as a connection.*
- [~] A refresh failure lands as `needs_reauth`, emits its event, and disables the affected nodes on the canvas.
      *Proven except the last clause. `refresh_or_use` returns four answers and the route
      maps them: `fresh`/`expired` (200/409), `refreshed` (200), `busy` (**202**, not an
      error — a peer holding the single-flight lock is a retry, and six concurrent callers
      produce one exchange), and `needs_reauth`/`unavailable` (502). Only a refusal the
      provider actually issued degrades the row: a 503 leaves it alone, because sending a
      reader to re-authorize for the provider's bad minute is worse than a stale token. The
      event `workflows.credential.needs_reauth` carries `credential_key`, which is what the
      canvas needs to disable the nodes. **"Disables the affected nodes" waits on REQ-086
      slice 2** — the canvas has nowhere to disable them.*
- [ ] The usage view matches a manual count of fixture workflows and node keys referencing a credential.
- [ ] Installing a third-party node package adds its nodes to the registry and palette without a restart; removal disables them and flags dependent workflows.
- [ ] A package failing the SDK validator is refused with the findings and nothing reaches the ledger.
- [ ] SDK scaffold → validate → pack yields an installable package whose fixtures pass for one action node and one credential-bearing node.
- [~] Credential search and filters return correct subsets, the expired-credential amber state appears for a past expiry, and the walkthrough traffic contains no fixture secret string.
      *Proven: the search, type, health and scope filters narrow the list and the URL carries
      them; the expired-credential amber state is computed (`effective_health`), not read from
      the column, so a token that expired while nothing was running still shows it; and the
      walkthrough sweeps a fixture secret across the DOM, the list and every API read. The
      credential screens are in the routes list and driven by the pass.*
- [ ] The credential access audit (REQ-125) records reads and tests with actor and time, and the detail link resolves.
- [ ] Library and credentials screens are keyboard navigable end to end and readable at 390 px.

### QA plan

Seed the registry, one third-party package fixture, one valid and one invalid API-key credential, an
OAuth fixture against a local provider stub, and a workflow using both. Walkthrough: search and filter in
`/workflows/nodes`, open a detail, add to canvas; create a credential (paste secret, test, save), open
detail, replace the secret, view usage, attempt a referenced delete (expect the guard), remove the
reference and delete; run the OAuth start/callback, disconnect and reconnect; install the third-party
package, confirm palette presence, remove it and confirm the canvas flags the dependent node. Visual
check: icons and capability chips render, secret inputs never show a value after reload, health chips
match the tested state, usage data is real.

### Slices

1. **Registry and discovery** — node and credential contracts, read-only registry, discovery endpoints, library screen. Done: palette and library render from the registry with schema-lint tests passing.
   *Shipped (`c3ec2d0`, `d3bf072`):* `crates/workflows/src/registry.rs` — the contract, the bundled
   registry and the lint; `apps/api/src/routes/node_types.rs` — seven read-only discovery
   endpoints; `/workflows/nodes` and `/workflows/nodes/<key>`; a `runNodeLibraryDepth` walkthrough
   pass. 52 crate tests + 7 API tests. **The palette itself (REQ-086 slice 2) is not wired to
   this registry yet** — it still reads w3's own list — so "the palette renders from the
   registry" is *not* proven and the slice stays open on that one clause.
2. **Credentials and storage** — tables, secret-store integration, CRUD with guards, usage view, audit. Done: no plaintext leaves the store and guard cases return their named errors.
   *Shipped (`f962d03`, `c5ae3fb`, `c2365a5`, `88ce5e3`): `0053_workflow_credentials.sql` — the
   two tables, with **no secret column at all** and `secret_ref` as the only handle;
   `crates/workflows/src/credentials.rs` — the entity, the four-value health and the
   `Settings` type that refuses to be built out of a field the type declares secret;
   `credential_store.rs` — CRUD, the usage probe derived from `workflows.graph`, and the delete
   guard inside the delete's own transaction with the row locked; `apps/api/src/routes/credentials.rs`
   — nine endpoints under two new permission keys; three admin screens; a walkthrough pass
   whose fixture secret must not survive the request. 72 crate + 10 API + 62 permission tests,
   `pnpm typecheck` 0 errors. **Still open on this slice:** the `ok: true` branch of the test
   hook (no bundled type can reach a provider from the API process), the write path into
   REQ-125's encrypted store — which returns `secret_store_unavailable` by design rather than
   inventing a scheme — and the referenced-delete refusal, which needs a fixture graph and
   arrives with the canvas.*
3. **OAuth and health** — start/callback, single-flight refresh, reauth state, canvas integration. Done: a fixture provider round-trips tokens and a forced refresh failure degrades correctly.
   *Shipped (`7edd4fa`, `f2a4b97`, `e2a7e17`, `faeb737`, `3e91777`, `7317cfb`, `77c60fb`): the
   flow's algebra, its persistence, the transport seam, the refresh caller, the four endpoints
   and the contract probe.*
   `crates/workflows/src/oauth.rs` — the signed `state` (HMAC-SHA256 over a
   `credential:issued:nonce` payload, ten-minute window, four *distinct* refusals so a client
   can tell a CSRF attempt from a person who was slow), PKCE derived `S256` and verified before
   the code is spent, the authorization URL built by *parsing* the endpoint so a `?tenant=` on a
   provider's endpoint survives, `TokenSet` (neither `Debug` nor `Serialize`, enforced by a
   compile-time test), and `RefreshLock` — single-flight per credential, because a refresh
   invalidates the old refresh token on most providers, so six concurrent nodes must produce
   one exchange. `LocalBox` seals the PKCE verifier for the length of one flow.
   `0054_workflow_oauth_flows.sql` + `oauth_store.rs` — the flow table with **no token column
   and no foreign key to the credential**, `state_hash` rather than the state (a state is a
   bearer value), and `claim_flow` as one `update … where status = 'pending'` so the database
   decides who spent it rather than the application's timing. `release_flow` exists so a
   provider timeout does not strand a flow in `completing` forever.
   `crates/workflows/src/oauth_client.rs` — the `OAuthClient` trait, so the `ok: true`
   branch of the test hook is reachable over a socket rather than asserted, and
   `token_set_from_status`, so the code exchange and the refresh cannot disagree about what a
   provider's answer means. `oauth_refresh.rs` — the caller, whose four-way answer
   (`Fresh`/`Refreshed`/`Busy`/`Reauth`) is the slice's real content: `Busy` is a retry, and a
   version that reads a lock timeout as a refusal marks a working credential `needs_reauth`.
   `apps/api/src/routes/credential_oauth.rs` — the four endpoints, with the callback
   unauthenticated by necessity and authenticated by the signed state.
   `scripts/qa/oauth-contract.sh` — a loopback provider and the probe that drives it.

   **One design change the callback forced.** The state's payload named only the credential,
   and the callback has no session to scope its lookup with — so the organization went into
   the *signed* bytes, and `verify_state` now returns a pair. A state minted for tenant A can
   no longer be replayed into tenant B even by an attacker who edits the query string.

   **One bug the tests caught that would have shipped silently.** `state_key` derived its HMAC
   key through `SecretBox::encrypt`, which produces a fresh random nonce per call, so signing
   and verifying used *different keys* and every callback failed as `credential_oauth_state` —
   a sentence about CSRF for what is really a key that never matched. The key is now a plain
   keyed hash of the raw installation material.

   `cargo test -p omnion-workflows --lib` 110 → **132**; `cargo test -p omnion-api --lib`
   **225 passed**; `pnpm typecheck` 0 errors.

   **Still open on this slice:** storing the token set (REQ-125 — the callback reports that
   refusal as a *failure* rather than as a connection), and `needs_reauth` reaching the canvas
   (REQ-086 slice 2 — the event carries `credential_key`, and the canvas has nowhere to
   disable nodes yet).
4. **Node packages and SDK** — ledger, install/remove via REQ-044, scaffold/validate/pack CLI, fixtures. Done: a fixture package installs, appears in the palette, and removal degrades instead of breaking.

### Risks / notes

- This entity must not become a second secrets manager: encryption, rotation and audit stay in REQ-125; if that slips, refuse credential writes rather than inventing a local scheme.
- OAuth refresh is the thundering-herd spot; a single-flight lock per credential plus jittered retry is required.
- Third-party packages run out of process (docs/09-N8N-TEARDOWN.md §13, lesson 14): the contract's `sandbox` flag keeps dynamic code out of the core process.
- Released node versions stay available for a deprecation window so workflows keep loading after an update.
- Credential keys appear in workflow exports (REQ-094); they are non-secret names, and an export must warn on collisions.
