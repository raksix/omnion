# REQ-087 — Node Library & Credential Catalog

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows` + plugins
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

Migrations `0031_workflow_node_packages.sql`, `0032_workflow_credentials.sql` (reserved band 0030–0039 for the workflow editor family, REQ-086–096; append-only ledger — take the next free number if taken).

```sql
-- 0031: what is installed (the registry itself is code)
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

-- 0032: credential metadata; the secret payload lives in the encrypted store (REQ-125)
create table workflow_credentials (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    key text not null, name text not null, type text not null,
    scope text not null default 'organization', sharing text not null default 'private',
    secret_id uuid,                                  -- reference into the encrypted store
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

Usage is derived from the graph (`jsonb_array_elements(w.graph -> 'nodes')` matched on
`params -> 'credential_key'`), never stored twice. Node params carry a credential *key*, never a value.

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

- [ ] `GET /api/v1/node-types` returns every node with ports, params schema, credential type and capabilities; the palette renders from it with no hard-coded list.
- [ ] A node detail carries docs link and version, and a deprecated node names its replacement in API and UI.
- [ ] Creating a credential stores no plaintext in the workflows schema (row inspection) and no response ever returns a secret value.
- [ ] Re-sending a secret field on `PATCH` fails with `credential_secret_write_only`; replace-secret is the only write path and is audited.
- [ ] **Test connection** returns ok for a valid credential and a masked failure for an invalid one.
- [ ] Deleting a referenced credential returns `credential_in_use` with the workflow list; a forced delete disables and names the dependent nodes.
- [ ] OAuth start → callback stores a token set, shows the connected identity, and rejects a tampered `state` with `credential_oauth_state`.
- [ ] A refresh failure lands as `needs_reauth`, emits its event, and disables the affected nodes on the canvas.
- [ ] The usage view matches a manual count of fixture workflows and node keys referencing a credential.
- [ ] Installing a third-party node package adds its nodes to the registry and palette without a restart; removal disables them and flags dependent workflows.
- [ ] A package failing the SDK validator is refused with the findings and nothing reaches the ledger.
- [ ] SDK scaffold → validate → pack yields an installable package whose fixtures pass for one action node and one credential-bearing node.
- [ ] Credential search and filters return correct subsets, the expired-credential amber state appears for a past expiry, and the walkthrough traffic contains no fixture secret string.
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
2. **Credentials and storage** — tables, secret-store integration, CRUD with guards, usage view, audit. Done: no plaintext leaves the store and guard cases return their named errors.
3. **OAuth and health** — start/callback, single-flight refresh, reauth state, canvas integration. Done: a fixture provider round-trips tokens and a forced refresh failure degrades correctly.
4. **Node packages and SDK** — ledger, install/remove via REQ-044, scaffold/validate/pack CLI, fixtures. Done: a fixture package installs, appears in the palette, and removal degrades instead of breaking.

### Risks / notes

- This entity must not become a second secrets manager: encryption, rotation and audit stay in REQ-125; if that slips, refuse credential writes rather than inventing a local scheme.
- OAuth refresh is the thundering-herd spot; a single-flight lock per credential plus jittered retry is required.
- Third-party packages run out of process (docs/09-N8N-TEARDOWN.md §13, lesson 14): the contract's `sandbox` flag keeps dynamic code out of the core process.
- Released node versions stay available for a deprecation window so workflows keep loading after an update.
- Credential keys appear in workflow exports (REQ-094); they are non-secret names, and an export must warn on collisions.
