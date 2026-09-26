# REQ-037 — Secrets Manager

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (secrets) + integrations
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Instead of leaving passwords in `.env` files:

```text
Secrets
├── OpenAI API Key
├── Stripe Secret
├── SMTP Password
├── LDAP Password
└── AWS Credentials
```

With integrations:

```text
Vault
AWS Secrets Manager
Azure Key Vault
Kubernetes Secrets
```

## Notes

- The secrets-provider pattern from docs/09 §9 (n8n's `secrets-provider-connection`) is prior
  art; secrets must never surface in logs, exports, or AI prompts (docs/06 §18 Data Guard).

## Implementation spec

### Scope (in / out)

**In**

- A first-class secret store: named secrets scoped `platform` · `organization` · `site` · `module`, versioned, and never returned in full by any read endpoint.
- Envelope encryption at rest — every version sealed with its own data key, data keys wrapped by the installation root key, and a `last four` hint stored separately so the UI can identify a value without decrypting it.
- External providers as references: a secret may live in Vault, AWS Secrets Manager, Azure Key Vault or Kubernetes Secrets; Omnion stores the pointer and resolves the value at use time with a short in-memory cache.
- Bindings: consumers declare which secret field they use (`ai.provider` · `smtp` · `storage.s3` · `identity.ldap` · `payments.stripe`), so a rotation shows its impact list before it happens.
- Rotation: policy in days, due/overdue states, reminders, rotate-now with optional approval and a grace window in which the previous version still resolves.
- A separate `secrets.reveal` permission with a required reason, an audit row per reveal and an optional second-approver gate.
- Import from a `.env` payload — key names are listed, values are stored, never echoed back — and one shared redaction helper used by logging, exports and AI prompts.

**Out**

- Being a general-purpose key management service for customer workloads; Omnion hands out no keys to external systems.
- Certificate issuance or renewal (deployment surface, REQ-024).
- Secrets inside plugin, theme or export bundles — values never leave through a file.
- Hardware-backed root keys (documented as a future extension, not built here).

### Screens (UI)

- `/secrets` — list. Columns: **Name · Scope · Environment · Provider · Version · Rotation · Bindings · Updated · Status** (`active` `rotation due` `overdue` `error`). Filters: scope, environment, provider, **rotation due** toggle, status, tag, free text. Bulk actions:
  **Rotate now · Set rotation policy · Tag · Delete** (typed confirmation; blocked while bindings exist unless the operator confirms the listed impact). Row click opens the detail.
- `/secrets/{id}` — detail with four tabs:
  - **Overview** — metadata, value shown as `••••••••` with a **Reveal** action (behind `secrets.reveal`, reason required, one-time display) and copy-name.
  - **Versions** — Version · Created · Created by · Reason · Status · Resolved by N bindings.
  - **Bindings** — Consumer · Field · Added, with add/remove controls.
  - **Access log** — When · Actor · Action · Reason · Result, read-only.
- Create/edit form fields: **Name** (required, `^[a-z][a-z0-9._-]{1,63}$`, unique per scope + environment) · **Value** (required on create, write-only, strength hint) · **Scope** ·
  **Environment** (`development` `staging` `production`) · **Description** (≤ 280) · **Provider** (`local` or a configured connection) · **Rotation policy** (days; 0 = never) · **Bindings** (multi-select of registered consumers) · **Tags**. Duplicate name fails inline with "a secret with this name already exists in this scope"; a production secret without rotation or without a binding shows a warning chip rather than blocking save.
- `/secrets/providers` — table: **Name · Kind · Endpoint · Auth reference · Health · Last checked**. Kinds: `local` `vault` `aws` `azure` `kubernetes`. Row actions: **Test · Edit · Disable**. Add form: Name · Kind · Endpoint · Auth secret (must point at an existing local secret) · Namespace/prefix · Enabled.
- `/secrets/import` — paste or upload a `.env` payload; preview table of key names with a per-key
  **store** checkbox and a proposed secret name. Values are never rendered back.
- States: masked values are a fixed-length mask, never a truncated real value; empty state on the list; a failing provider test shows an error banner with the provider's message; skeletons while loading; a denied reveal shows "permission required" inline instead of a raw error.
- Keyboard: `/` search, `n` new secret, `r` rotate the focused secret, `Esc` closes the drawer. Mobile: the detail tabs stack vertically, the list becomes cards.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/secrets` | Metadata list (values never included) | `secrets.read` |
| POST | `/api/v1/secrets` | Create a secret and its first version | `secrets.manage` |
| GET | `/api/v1/secrets/{id}` | Metadata, versions and bindings | `secrets.read` |
| PATCH | `/api/v1/secrets/{id}` | Rename, describe, re-scope, rotation policy, tags | `secrets.manage` |
| DELETE | `/api/v1/secrets/{id}` | Delete secret, versions and bindings | `secrets.manage` |
| POST | `/api/v1/secrets/{id}/versions` | Write a new version (value in the body) | `secrets.manage` |
| GET | `/api/v1/secrets/{id}/versions` | Version history without values | `secrets.read` |
| POST | `/api/v1/secrets/{id}/rotate` | Rotate now (approval when configured) | `secrets.manage` |
| POST | `/api/v1/secrets/{id}/reveal` | Return the value once; reason required | `secrets.reveal` |
| GET | `/api/v1/secrets/{id}/access-log` | Reads, rotations and reveals | `secrets.read` |
| POST | `/api/v1/secrets/{id}/bindings` | Attach a consumer field | `secrets.manage` |
| DELETE | `/api/v1/secrets/{id}/bindings/{binding_id}` | Detach a consumer field | `secrets.manage` |
| POST | `/api/v1/secrets/import` | Parse a `.env` payload into key names | `secrets.manage` |
| GET | `/api/v1/secrets/export-metadata` | Names, scopes, providers, rotation state | `secrets.read` |
| GET | `/api/v1/secret-providers` | List provider connections | `secrets.read` |
| POST | `/api/v1/secret-providers` | Add a provider connection | `secrets.providers.manage` |
| PATCH | `/api/v1/secret-providers/{id}` | Edit a provider connection | `secrets.providers.manage` |
| POST | `/api/v1/secret-providers/{id}/test` | Reachability and credential check | `secrets.providers.manage` |

Every response uses an explicit serializer: a value field exists only in the `reveal` response and in request bodies. Error responses never echo a submitted value, and validation messages name the field, never the content.

### Data model

`database/migrations/0012_secrets.sql`:

- `secret_providers` — `id uuid pk`, `organization_id uuid`, `name text not null`, `kind text not null check (kind in ('local','vault','aws','azure','kubernetes'))`, `endpoint text`, `auth_secret_id uuid references secrets(id)`, `namespace text`, `config jsonb not null default '{}'`, `enabled boolean not null default true`, `health text not null default 'unknown' check (health in ('unknown','ok','failing'))`, `last_checked_at timestamptz`, `created_by uuid`, `created_at timestamptz not null default now()`. Unique `(organization_id, name)`.
- `secrets` — `id uuid pk`, `organization_id uuid`, `scope text not null` `check (scope in ('platform','organization','site','module'))`, `environment text not null` `check (environment in ('development','staging','production'))`, `name text not null`, `description text`, `provider_id uuid not null references secret_providers(id)`, `rotation_days int not null default 0 check (rotation_days between 0 and 3650)`, `next_rotation_at timestamptz`, `current_version int not null default 1`, `last_four text not null default ''`, `tags text[] not null default '{}'`, `created_by uuid`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`. Unique `(organization_id, scope, environment, name)`.
- `secret_versions` — `id uuid pk`, `secret_id uuid not null references secrets(id) on delete cascade`, `version int not null`, `ciphertext bytea`, `key_id text not null`, `checksum text not null`, `reason text`, `status text not null default 'active'` `check (status in ('active','retired'))`, `created_by uuid`, `created_at timestamptz not null default now()`, `retired_at timestamptz`. Unique `(secret_id, version)`.
- `secret_bindings` — `id uuid pk`, `secret_id uuid not null references secrets(id) on delete cascade`, `consumer text not null`, `field text not null`, `created_at timestamptz not null default now()`. Unique `(secret_id, consumer, field)`.
- `secret_access_log` — `id bigserial pk`, `secret_id uuid not null`, `version int`, `actor_user_id uuid`, `actor_type text not null`, `action text not null` `check (action in ('read','write','rotate','reveal','delete','bind'))`, `reason text`, `result text not null`, `ip_address text`, `created_at timestamptz not null default now()`.

Indexes: `secrets_name_idx (organization_id, name)`, `secrets_rotation_idx (next_rotation_at) where next_rotation_at is not null`, `secret_versions_secret_idx (secret_id, version desc)`, `secret_bindings_consumer_idx (consumer)`, `secret_access_log_secret_idx (secret_id, created_at desc)`.

Each version is sealed under a per-version data key; the data key is wrapped by the installation root key named in `key_id`. Rotation writes a new version and retires the old one after the grace window instead of deleting it in place.

### Events

- `secret.created` · `secret.updated` · `secret.deleted`
- `secret.rotated` — `{name, scope, environment, version}`
- `secret.revealed` — `{name, actor, reason}`; the value is never part of a payload
- `secret.rotation.due` · `secret.rotation.overdue` — emitted by the scheduler
- `secret.provider.health_changed` — `{name, kind, health}`
- `secret.binding.added` · `secret.binding.removed`

Webhook relevance: rotation reminders and provider health are the two payloads an operator wants in a chat or ticket tool. Payloads carry names and identifiers only, and the shared redaction helper runs over every payload before it is stored, so the bus, exports and AI context stay clean.

### Acceptance criteria

- [ ] Creating a secret stores a value that no read endpoint returns.
- [ ] `GET /secrets` and `GET /secrets/{id}` never contain a plaintext value in any field.
- [ ] Reveal requires `secrets.reveal` plus a reason and writes an access-log row.
- [ ] A caller without `secrets.reveal` receives `403` on reveal, with the reason shown inline in the UI.
- [ ] Rotation writes a new version, marks the previous one `retired` after the grace window, and the newest value is the one consumers resolve.
- [ ] A production secret with `rotation_days = 0` shows a warning chip on the list and detail.
- [ ] Deleting a secret with bindings is blocked until the operator confirms the listed impact.
- [ ] `.env` import lists key names, stores the selected ones, and never returns a value in the preview.
- [ ] Duplicate names inside scope + environment fail with an inline validation message.
- [ ] Providers can be added, tested, edited and disabled; a failed test shows the provider message and does not change the stored configuration.
- [ ] An external provider secret resolves at use time; a provider outage fails the consumer closed rather than serving a stale value.
- [ ] Logs, webhook payloads and AI prompts contain masked values only (asserted in a test that greps recorded output for the fixture value).
- [ ] Every mutation writes an audit row.
- [ ] `cargo test --workspace` and `pnpm typecheck && pnpm build` pass.
- [ ] The QA walkthrough exercises list, detail, create, rotate, reveal-denied and delete without a high finding.

### QA plan

- The browser walkthrough visits `/secrets`, opens a fixture secret's four tabs, creates a new secret through the form, rotates it, deletes it, and visits `/secrets/providers` and `/secrets/import`.
- Controls to exercise: filters and the rotation-due toggle, the reveal button on a user without the permission (expects the inline denial) and on a user with it (expects a single display and a new access-log row), rotate with confirmation, binding add/remove, provider **Test**, and the `.env` import preview.
- Assertions: the rendered DOM never contains the fixture value; the access log gains exactly one row per reveal; the version table grows by one per rotation.
- The visual check must see masked values, warning chips for missing policies, readable tables at 1280 px, card layout under 640 px, and no value ever echoed into a toast or error banner.

### Slices

1. **Store + crypto + read API** — migration, sealing/unsealing helper with the root key, secret CRUD routes, `/secrets` list and `/secrets/{id}` overview.
   *Done:* a stored value round-trips through rotate/resolve in tests, and no read response contains it.
2. **Reveal + audit + access log** — reveal route with reason, denial path, access-log tab.
   *Done:* reveal is audited, denied without the permission, and the tab shows the history.
3. **Providers** — provider CRUD, health test, external resolution with fail-closed behaviour, `/secrets/providers`.
   *Done:* a fixture provider serves a value, an unreachable provider fails the consumer and flips health to `failing`.
4. **Rotation, bindings, import and redaction** — policy fields, due/overdue states, rotation reminder event, binding impact list, `.env` import, the shared redaction helper wired into logging and payloads.
   *Done:* the walkthrough covers the whole screen set and the redaction test passes.

### Risks / notes

- Losing the installation root key makes every locally stored secret unrecoverable; the setup and provider screens must state this plainly.
- Reveal is the single path a value travels through the API — keep it rate-limited, reason-bound and audited on every call, and never cache its response.
- External provider outages must fail closed for consumers that need a value; a stale cache that silently serves an old credential is worse than an error.
- Keep the previous version resolvable for the grace window so a rotation does not break a running consumer mid-flight.
- The `.env` import is the most likely place for an operator to paste a value into a chat or an issue; the UI must never reflect the payload back.
- Redaction has one implementation, shared with the audit trail (REQ-039) and the AI data guard (docs/06-AI-HUB.md §18) — two pattern lists would drift.
