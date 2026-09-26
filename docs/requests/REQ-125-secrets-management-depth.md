# REQ-125 — Secrets & Credential Management

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Credentials that never sit in a text file.

- Encryption at rest for all stored credentials (per-installation key), with rotation support.
- Credential entities: API keys, OAuth tokens, SMTP accounts, payment keys, SSH keys.
- External secret providers (Vault-style, file-based, env-based) with instance-level credential assignment.
- Deployment keys for CI and remote environments; helper-mediated access so plaintext is never returned to the browser.
- Access audit: who read which secret, when, and from where.

## Implementation spec

Depth layer over the secrets store of REQ-037 (secret CRUD, envelope encryption, provider connections, bindings, rotation policy, `secrets.reveal`). This request adds the **key hierarchy with rotation**, **typed credential entities**, **instance-level slot assignment**, **short-lived leases with a local helper**, **deployment keys** and **audit depth**. New code lives in `crates/secrets` (key ring, validators, lease broker, assignment resolver) plus the helper subcommand of the CLI under `tools/cli`; the store and permission names from REQ-037 are reused, never duplicated.

### Scope (in / out)

**In**

- **Key hierarchy.** Installation root keys are themselves wrapped by an operator-supplied key-encryption key delivered through the environment or an operator-controlled key file — never stored by the platform. Root key rotation is an online ceremony: generate, wrap, re-wrap every secret version's data key in a resumable background job, flip the active key, retire the old one. Unsealing reads the `key_id` already recorded per version (REQ-037), so a version sealed under a retired key keeps resolving during and after the re-wrap.
- **Typed credentials.** A credential profile extends a secret with a `kind` (`api_key` `oauth_token` `smtp_account` `payment_key` `ssh_key`) and structured **non-secret** fields (endpoint, username, port, token expiry, key fingerprint, fingerprint algorithm). Per-kind validators: SMTP connect + greeting over the configured port, payment key read-only account call when the provider exposes one, SSH key format + fingerprint recomputation, OAuth token expiry parse, API key format/prefix check. Validation runs on demand and on a schedule; results are stored and shown as a chip.
- **Provider depth.** `file` and `env` providers join the kinds of REQ-037 as **read-only** sources: a file provider names a path (host path or mounted file) and a key name inside it, an env provider names a variable; both resolve through the helper on the machine that runs the workload and Omnion stores only the pointer, marked `read-only · managed outside`. A provider that is read-only can never be written to, and the UI says so.
- **Instance-level assignment.** `credential_slots` bind a slot (`ai.provider` `smtp` `payments.stripe` `storage.s3` `ssh.release` `identity.ldap`) for a scope (environment, site, module) to a primary and an optional fallback secret. Consumers resolve through the slot, never through a hard-coded secret id, so swapping a credential is a slot update. The panel shows which credential each slot resolves — name, version, last resolved — never a value.
- **Leases.** `POST /secrets/{id}/lease` issues an opaque, short-lived, use-capped credential lease (default TTL 15 min, maximum 24 h) to a named consumer. The lease token is not the secret. Redemption happens over the loopback helper path by a machine identity, and the returned plaintext is written only into a child process environment or a mode-0600 temporary file that is removed on exit. Outstanding leases are listed, revocable, and revoked automatically when the deployment centre reports a deploy for the environment they were issued for.
- **Deployment keys.** Scoped machine credentials for CI and remote environments. A deployment key can lease secrets inside its scope and environment; it can never reveal, list values or read another environment. Each use is logged with the pipeline identity, the source address and the lease id, and a key expires on a date the operator sets.
- **Audit depth.** The REQ-037 access log is extended with denials, lease redemptions, root key operations, slot changes and deployment key uses, plus anomaly flags: reveal outside configured hours, a burst of reveals, first access from a new network, reveal by a principal that never held the secret. Flags are advisory records with an acknowledge action; one optional hard rule (for example "production reveals require a second approver") may block.
- **SIEM feed.** A filtered audit export (webhook, JSON lines or syslog-shaped) that carries metadata only — never a value, never a masked fragment.

**Out**

- Being a key management service for customer workloads; Omnion hands out no keys to external systems.
- Hardware-backed root keys, certificate issuance and renewal (future extension, documented only).
- Secrets inside plugin, theme or export bundles — no value ever leaves through a file.
- A general-purpose vault UI; the screen set stays a platform surface.

### Screens (UI)

| Route | Purpose |
|---|---|
| `/secrets/root-key` | Key ring: active and retired keys, seal/unseal self-check, rotation ceremony with re-wrap progress |
| `/secrets/credentials` | Typed credential list with kind, validation state, last validated, next validation |
| `/secrets/slots` | Slot assignment matrix: scope, slot, primary, fallback, state, last resolved |
| `/secrets/leases` | Active and recent leases: secret, consumer, issued to, expires, uses, revoke |
| `/secrets/deploy-keys` | Deployment keys with scope, environment, expiry, last use; create drawer shows the value once |
| `/secrets/audit` | Full audit incl. denials and anomalies, filters, acknowledge, export |

- `/secrets/{id}` gains two tabs beside the REQ-037 set: **Credential** (kind, non-secret fields, validator result, validate action) and **Leases** (open leases for this secret). The masked-value display and the one-time reveal rule from REQ-037 are unchanged.
- Root key screen: rotation is a three-step wizard (confirm operator key is available → start re-wrap → verify). Progress is a real counter (versions re-wrapped of total) with pause, resume and a resume note if the process restarts; the screen states plainly that losing the operator key makes local secrets unrecoverable.
- Credential create wizard: pick kind → fill non-secret fields → value entry (write-only) → validator runs → save. A failing validation saves the credential but keeps the chip red with the provider message; it never blocks storage of a credential the operator knows is temporarily unreachable.
- Slot editor: primary and fallback must differ; removing a slot that a consumer is actively resolving asks for confirmation and names the consumer.
- Lease list: expires-in countdown, use counter, `Revoke` with a reason; a revoked lease redeemed again shows the denial in the audit, not an error page.
- Deployment key create: name, environment, scope list, expiry (required), optional IP allow-list; value shown once with the same copy panel rule as the gateway keys (REQ-040).
- States: empty states per screen; skeletons; a denied action renders inline ("permission required" / "this deployment key cannot reveal"); every error carries a request id. A read-only provider shows an explanation instead of a disabled edit even if the operator has permission.
- Keyboard: `/` search, `n` new credential, `r` rotate (list context), `Esc` closes drawers. Mobile: tables become cards, the wizard is one column.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/secrets/root-key` | Key ring state, active key, re-wrap coverage | `secrets.read` |
| POST | `/api/v1/secrets/root-key/rotate` | Start the rotation ceremony | `secrets.root.manage` |
| GET | `/api/v1/secrets/root-key/rewrap-jobs/{id}` | Re-wrap progress, pause/resume state | `secrets.read` |
| POST | `/api/v1/secrets/root-key/rewrap-jobs/{id}/pause` | Pause a running re-wrap | `secrets.root.manage` |
| GET | `/api/v1/secrets/credentials` | Typed credentials with validation state | `secrets.read` |
| POST | `/api/v1/secrets/{id}/validate` | Run the kind validator now | `secrets.manage` |
| GET | `/api/v1/credential-slots` | Slot assignments | `secrets.read` |
| PUT | `/api/v1/credential-slots/{scope}/{slot}` | Assign primary and fallback | `secrets.assign` |
| POST | `/api/v1/secrets/{id}/lease` | Issue a lease to a consumer | `secrets.lease` |
| GET | `/api/v1/secret-leases` | Active and recent leases | `secrets.read` |
| POST | `/api/v1/secret-leases/{id}/revoke` | Revoke with a reason | `secrets.lease` |
| POST | `/api/v1/secret-leases/{id}/redeem` | Helper-only redemption; value in this response only | machine identity (deployment key) |
| GET | `/api/v1/deployment-keys` | List keys (metadata only) | `secrets.deploy_keys.read` |
| POST | `/api/v1/deployment-keys` | Create; value shown once | `secrets.deploy_keys.manage` |
| POST | `/api/v1/deployment-keys/{id}/revoke` | Revoke immediately | `secrets.deploy_keys.manage` |
| DELETE | `/api/v1/deployment-keys/{id}` | Delete a revoked key record | `secrets.deploy_keys.manage` |
| GET | `/api/v1/deployment-keys/{id}/uses` | Use log: pipeline, address, lease, result | `secrets.deploy_keys.read` |
| GET | `/api/v1/secrets/audit` | Audit incl. denials and anomalies | `secrets.audit` |
| PATCH | `/api/v1/secrets/audit/{id}/acknowledge` | Acknowledge an anomaly flag | `secrets.audit` |
| GET | `/api/v1/secrets/audit/export` | SIEM-shaped export, metadata only | `secrets.audit` |

Errors: `403` on any scope escalation (a deployment key redeeming outside its environment), `409` when a slot would map a secret to itself as fallback, `410` on an expired lease, `422` from a validator, `423` while a re-wrap job holds the key ring mid-flip for more than the configured grace. Serializers stay explicit as in REQ-037: a value field exists only in `reveal` and `redeem` responses.

### Data model

Migration: `database/migrations/0026_secrets_depth.sql` (next free slot at tick time), additive over the REQ-037 tables, with a down script per the REQ-129 policy.

- `secret_root_keys` — `id uuid pk`, `key_id text not null unique`, `algorithm text not null default 'aes-256-gcm'`, `wrapped_material bytea not null` (root key wrapped by the operator key-encryption key), `state text not null default 'active' check (state in ('active','retiring','retired'))`, `created_by uuid`, `created_at`, `retired_at`.
- `secret_rewrap_jobs` — `id uuid pk`, `from_key_id text not null`, `to_key_id text not null`, `status text not null default 'running' check (status in ('running','paused','succeeded','failed','cancelled'))`, `total_versions int not null default 0`, `done_versions int not null default 0`, `error text`, `started_by uuid`, `started_at`, `finished_at`.
- `secret_credentials` — extends a secret 1:1: `secret_id uuid pk references secrets(id) on delete cascade`, `kind text not null check (kind in ('api_key','oauth_token','smtp_account','payment_key','ssh_key'))`, `fields jsonb not null default '{}'` (non-secret: host, port, username, expires_at, fingerprint, fingerprint_algorithm), `validation_state text not null default 'unknown' check (validation_state in ('unknown','ok','failing','unsupported'))`, `validated_at`, `validation_detail text`, `validate_interval_hours int not null default 24`.
- `credential_slots` — `id uuid pk`, `scope text not null check (scope in ('environment','site','module'))`, `scope_id text not null`, `slot text not null check (slot in ('ai.provider','smtp','payments.stripe','storage.s3','ssh.release','identity.ldap'))`, `primary_secret_id uuid not null references secrets(id) on delete restrict`, `fallback_secret_id uuid references secrets(id) on delete set null`, `state text not null default 'active' check (state in ('active','disabled'))`, `last_resolved_at`, `assigned_by uuid`, `assigned_at`. Unique `(scope, scope_id, slot)`; check `fallback_secret_id is distinct from primary_secret_id`.
- `secret_leases` — `id uuid pk`, `secret_id uuid not null references secrets(id) on delete cascade`, `version int`, `consumer text not null`, `issued_to text not null` (`user|deployment_key|helper`), `issued_to_id uuid`, `token_hash text not null unique`, `environment text`, `max_uses int not null default 1`, `used_count int not null default 0`, `expires_at timestamptz not null`, `revoked_at`, `revoke_reason text`, `issued_by uuid`, `issued_at`.
- `deployment_keys` — `id uuid pk`, `organization_id uuid`, `name text not null`, `environment text not null`, `scopes text[] not null default '{}'`, `token_hash text not null`, `expires_at timestamptz not null`, `ip_allowlist cidr[] not null default '{}'`, `last_used_at`, `revoked_at`, `created_by uuid`, `created_at`. Unique `(organization_id, name)`. Uses are rows in the extended access log, not a second ledger.
- `secret_audit_anomalies` — `id bigserial pk`, `secret_id uuid`, `pattern text not null check (pattern in ('off_hours_reveal','reveal_burst','new_network','unfamiliar_principal'))`,` `severity text not null default 'advisory'`, `detail jsonb not null default '{}'`, `acknowledged_by uuid`, `acknowledged_at`, `created_at`. Detector thresholds live in the settings row added by this migration (`reveal_burst_per_hour`, `business_hours`, `hard_rule` flags).
- `secret_access_log` (REQ-037) is extended additively: `action` gains `lease`, `redeem`, `deny`, `root_rotate`, `slot_change`, `deploy_key_use`; new nullable columns `lease_id uuid`, `deployment_key_id uuid`, `request_id uuid`, `pipeline text`.
- Indexes: `secret_leases_active_idx (expires_at) where revoked_at is null`, `secret_leases_secret_idx (secret_id, issued_at desc)`, `deployment_keys_env_idx (environment, revoked_at)`, `credential_slots_lookup_idx (scope, slot)`, `secret_rewrap_jobs_status_idx (status) where status in ('running','paused')`.

### Events

- **Emitted:** `secret.root_key.rotated`, `secret.rewrap.progress` (throttled), `secret.rewrap.completed`, `secret.lease.issued`, `secret.lease.redeemed`, `secret.lease.revoked`, `secret.lease.expired`, `secret.access.denied`, `secret.audit.anomaly`, `credential.validation.failed`, `credential.validation.recovered`, `credential_slot.assigned`, `deployment_key.created`, `deployment_key.used`, `deployment_key.revoked`.
- **Consumed:** `deployment.started` (REQ-024) revokes outstanding leases for that environment so a redeploy never runs on a credential the operator just replaced; `secret.rotated` (REQ-037) marks dependent credentials for revalidation; provider health changes re-run validators for credentials that resolve through the affected provider.
- Webhook relevance: rotation completion, validation failures and access denials are the payloads an operator wires into chat; `secret.lease.*` is high volume and is aggregated per secret and hour. Every payload carries names, ids and counts — the shared redaction helper of REQ-037 runs before storage so the bus, exports and AI context stay clean.
- Notification relevance: an overdue root key rotation and a production credential failing validation notify holders of `secrets.root.manage` through the REQ-021 router.

### Acceptance criteria

- [ ] `database/migrations/0026_secrets_depth.sql` applies on a fresh and a populated database, and its down script reverses it.
- [ ] Secrets remain unreadable without the operator key; the root key screen states this and the seal check proves it.
- [ ] Root key rotation re-wraps every existing version, and consumers keep resolving during and after it (test resolves a secret mid-rotation).
- [ ] A paused re-wrap resumes after a process restart without double-wrapping or losing position.
- [ ] All five credential kinds can be created; non-secret fields render, values never do.
- [ ] A validator failure stores the provider message and a red chip without blocking the save.
- [ ] A credential resolved through a slot returns the primary; removing the primary falls back to the fallback with an event.
- [ ] A slot cannot hold the same secret as primary and fallback (`409`).
- [ ] A lease has a TTL and a use cap; redeeming beyond either is refused and audited as a denial.
- [ ] Redeeming a revoked or unknown lease token returns `410`/`401` with a request id and writes a denial row.
- [ ] A deployment key can lease inside its environment and is refused (`403`) outside it, including on `reveal`.
- [ ] A deployment key past its expiry is refused (`401`).
- [ ] `POST /secrets/{id}/lease` never returns the value; only `redeem` does, and only for a machine identity.
- [ ] `deployment.started` revokes live leases for the environment and the lease list shows them revoked with the reason.
- [ ] The helper injects a leased value into a child process without writing it to the shell history or a log; the temp-file path is mode 0600 and removed on exit.
- [ ] Read-only (`file`, `env`) providers cannot be written to from the API (`405`/`422`) and the UI explains why.
- [ ] Audit rows exist for read, write, rotate, reveal, deny, lease, redeem, slot change and deployment key use, with actor, address and request id.
- [ ] Anomaly detection flags an off-hours reveal and a reveal burst in a scripted test, and the acknowledge action persists.
- [ ] The SIEM export contains metadata only (asserted by a test that greps the payload for the fixture value).
- [ ] `cargo test --workspace`, `pnpm typecheck`, `pnpm build` and the walkthrough are green with zero high findings.

### QA plan

The browser walkthrough visits `/secrets/root-key`, `/secrets/credentials`, `/secrets/slots`, `/secrets/leases`, `/secrets/deploy-keys` and `/secrets/audit`, plus the new tabs on a secret detail. Controls to exercise: start and pause a re-wrap on the QA stack (a disposable environment with a small key ring), run a validator against the mock SMTP service shipped with the QA stack, assign a slot and swap primary/fallback, issue a lease and revoke it, redeem a lease through the helper in the same pass, create a deployment key and confirm the once-only value panel, and acknowledge an anomaly fixture.

Assertions that must hold in the same pass: the rendered DOM never contains a fixture value (document scan plus response-body grep), the audit gains exactly one row per operation, a refunded lease redemption shows as denied, and the helper's child process sees the value while its own log does not. The visual check must see masked values, chips distinguishable without colour, a real re-wrap counter, readable tables at 1280 px, card layout under 640 px, and no value ever echoed into a toast, tooltip or error banner.

### Slices

1. **Key hierarchy + rotation.** Migration, root key ring, wrap/unwrap helper, re-wrap job with pause/resume, `/secrets/root-key`, seal self-check, events. *Done when:* a rotation completes on the QA stack with all versions re-wrapped and a consumer resolves throughout.
2. **Typed credentials + slots.** Credential profiles with non-secret fields, validators (mock SMTP and a payment-format check), `/secrets/credentials`, slot model and resolver, `/secrets/slots`. *Done when:* a slot swap changes which credential a consumer resolves, with an event and an audit row.
3. **Leases + helper + deployment keys.** Lease table and endpoints, loopback redemption, helper subcommand with env injection and 0600 temp-file mode, lease auto-revocation on deploy, `/secrets/leases`, `/secrets/deploy-keys` with scoped machine identities. *Done when:* CI-shaped redemption works from a deployment key inside its environment and is refused outside it.
4. **Audit depth + SIEM + anomalies.** Access log extension, denial rows everywhere, anomaly detectors and acknowledge, filtered export, notification wiring. *Done when:* a scripted off-hours reveal produces an anomaly row, an acknowledge persists, and the export carries no value.

### Risks / notes

- Losing the operator-supplied key-encryption key makes every locally stored secret unrecoverable; setup and the rotation wizard must say this plainly, and the seal self-check gives an early warning before a rotation.
- The re-wrap job is the one long-running writer in the secrets store: batch small, resume from a cursor, and never hold a lock a normal read needs — a lease redemption during a re-wrap must succeed on the old key until that version is re-wrapped.
- Redemption is the only path plaintext travels through the API. It is loopback-scoped, machine-identity bound, use-capped, audited and never cached or logged; anything that would put a value in a browser response is out of scope by design.
- Environment and file providers are read-only bridges to credentials managed outside Omnion. They must never grow a write path, or the platform becomes a second, worse copy of the operator's file store.
- A deployment key is a machine credential in CI, so it will leak eventually. Keep scopes narrow, require expiry, log every use with the pipeline name, and make revocation one click.
- Anomaly signals are advisory by default. A hard rule that blocks reveals can lock an incident responder out at the worst moment, so it ships off and documented.
- Two redaction implementations would drift: this request reuses the single helper from REQ-037 and the audit trail (REQ-039) rather than adding patterns.
