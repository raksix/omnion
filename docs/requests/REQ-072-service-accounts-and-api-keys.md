# REQ-072 — Service Accounts & API Keys

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Non-human identities.

- Service account registry (name, purpose, owner, last used, status) with their own grants and explicit denies.
- API keys per service account: create, scope, rotate, revoke, expiry, usage counter.
- Documented service identities: CI/CD, analytics, backup worker, AI agent, external CRM.
- Key authentication path (header), audit of every key-authenticated request.
- Panel screens for keys, rotation reminders, and last-used visibility.

## Implementation spec

> **Where:** `crates/permissions` (service-account store and key verification) · `crates/identity` (key hashing helper shared with sessions) · `apps/api/src/auth.rs` + `apps/api/src/routes/iam.rs` · `apps/admin/features/iam/service-accounts/**` (new screens) · **Migration:** `database/migrations/0018_service_accounts_and_api_keys.sql` (next free number at land time; the ledger is append-only — take the next free slot if one is already used) · **Admin routes:** `/settings/iam/service-accounts`, `/settings/iam/service-accounts/{id}` · **Permission family:** `iam.serviceaccounts.*` · **Depends on:** REQ-006 (`service_account` is a binding subject and key fields were sketched there — one table per concept), REQ-070 (scopes a service account can be bound at), REQ-067 (role picker in the grants tab), REQ-021 notifications (rotation reminders), REQ-012 security centre (consumes the key events).

### Scope (in / out)

**In**

- **Registry:** one row per machine identity — name (unique per organization, case-insensitive), purpose text, owner (a user, mandatory so every non-human identity has a human to answer for it), status `active | disabled`, optional expiry, last-used timestamp, key count, created/updated, archived flag. Owner deactivation does not silently orphan the account: it is flagged `owner_missing` and listed in the rotation/attention panel until reassigned.
- **Grants and explicit denies:** a service account is a first-class subject in the REQ-006 binding model (`subject_type = 'service_account'`), so it receives roles at any REQ-070 scope and can carry explicit denies; its effective set resolves through the same `crates/permissions::authorize` path as a user and is shown on the grants tab with the source of every entry.
- **Key lifecycle:** create (secret shown once), narrow (optional per-key permission subset and IP allowlist), rotate (new key with an overlap window during which the previous key still works), revoke (immediate), expiry (`expires_at`), usage counter and last-used-at per key. Only one active key per key *name*; rotation keeps the family link (`rotated_from_key_id`) so the usage history stays readable.
- **Storage rule:** a key is `prefix + secret`; only the prefix (lookup) and a hash of the full key (verification) are stored. The plaintext is returned exactly once in the creation/rotation response, is never logged, never echoed in an event and never retrievable afterwards — the API answers `410 Gone` on a re-read attempt with a message telling the operator to rotate.
- **Authentication path:** `Authorization: Bearer <prefix>.<secret>` on any `/api/v1` route resolves the key, checks status, expiry, IP allowlist and the per-key permission subset, then authorises through the same guard as a session. A service-account request has no session, no CSRF surface, cannot call sign-in, password, MFA or session routes, and cannot hold interactive-only keys (`users.impersonate`, `iam.providers.*`, `iam.serviceaccounts.*` are refused on a binding to this subject with a field-level error naming the key).
- **Auditability:** every key-authenticated request writes an audit entry (key id, service account, method, path, decision, status, client IP) with the secret never present; denials write an entry too. Daily rollups keep the usage screen cheap.
- **Documented identities:** reusable create templates — `CI/CD`, `analytics`, `backup worker`, `AI agent`, `external CRM` — each a name + purpose + suggested role set preset; a preset never grants silently, it only pre-fills the form.
- **Reminders:** a scheduled job emits `iam.serviceaccount_key_expiring` at 14, 7 and 1 days before expiry and `iam.serviceaccount_key_stale` when a key has not authenticated for 90 days; the notification centre renders them and the panel shows a badge.

**Out (tracked elsewhere)**

- Human sign-in, SSO and JIT provisioning → REQ-065; MFA and passkeys for humans → REQ-066; per-route rate limits and the public API gateway → REQ-040; storage of third-party provider credentials → REQ-037 secrets manager (service-account keys are our own hashed secrets and never live in the secrets store); webhook signing secrets → REQ-016; AI agent permission bundles → REQ-001 / REQ-047; long-term audit retention tiering → REQ-039.

### Screens (UI)

Nav entry under the existing IAM section: **Service accounts**.

| Route | Screen |
|---|---|
| `/settings/iam/service-accounts` | Registry list with status, owner, key count, last used, expiry badge |
| `/settings/iam/service-accounts/{id}` | Detail — tabs `Grants`, `Keys`, `Usage`, `Audit` |
| `/settings/iam/service-accounts/{id}/keys/{key_id}` | Key detail — prefix, window, IP allowlist, per-key subset, usage series |

- **List** columns `Name` (monogram chip), `Purpose`, `Owner`, `Status`, `Keys` (active/total), `Last used`, `Expires`, `Updated`; filters: search (250 ms debounce), status, owner, expiring ≤ 30 days, unused ≥ 90 days, has explicit deny; bulk actions: disable, enable, revoke all keys, reassign owner, export; row actions: open, edit drawer, create key, rotate, disable. A persistent attention band lists accounts with `owner_missing`, a key expiring in ≤ 14 days, or a stale key.
- **Create wizard (3 steps):** identity (template chip row, name, purpose, owner, expiry) → grants (role picker from REQ-067 with a scope picker from REQ-070, plus an optional explicit deny list) → first key (name, expiry, IP allowlist, permission subset). The review step states plainly that the secret is shown once.
- **Secret dialog:** the full key in a read-only field with `Copy`, a one-time acknowledgement checkbox that enables `Done`, a warning that closing the dialog loses it, and a `Download .env` shortcut that writes only the variable line. No screenshot-friendly sneaky hide — the operator is told, not tricked.
- **Keys tab** columns `Key`, `Prefix`, `Created`, `Expires`, `Last used`, `Requests (30 d)`, `Status` (`active`, `rotating`, `expired`, `revoked`, `disabled`); row actions rotate, revoke, edit allowlist; the rotate dialog shows the overlap window and the exact moment the old key stops.
- **Usage tab:** request count per day as a compact bar strip, top paths, denial count, last 10 denials with the reason; empty state when the key has never been used. **Audit tab** reuses the shared audit table component.
- **States, keyboard, mobile:** `EmptyState` with the primary action, `LoadingTable` skeleton, inline error with retry; `/` focuses search, `j`/`k` move focus, `enter` opens, `e` edits, `?` opens the shortcut sheet; below `lg` tables become cards and the wizard is single-column with a sticky footer.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET, POST | `/api/v1/iam/service-accounts` | List with counts / create | `iam.serviceaccounts.read` / `iam.serviceaccounts.manage` |
| GET, PATCH, DELETE | `/api/v1/iam/service-accounts/{id}` | Detail / rename, purpose, owner, expiry, status / archive | `iam.serviceaccounts.read` / `iam.serviceaccounts.manage` |
| GET, POST, DELETE | `/api/v1/iam/service-accounts/{id}/bindings` | Grants and explicit denies / attach a role at a scope / revoke | `iam.bindings.read` / `iam.bindings.manage` |
| GET | `/api/v1/iam/service-accounts/{id}/effective-permissions` | Resolved set with sources | `iam.serviceaccounts.read` |
| GET, POST | `/api/v1/iam/service-accounts/{id}/keys` | Key list (prefix, window, usage) / issue a key | `iam.serviceaccounts.read` / `iam.serviceaccounts.manage` |
| POST, DELETE | `/api/v1/iam/service-accounts/{id}/keys/{key_id}/rotate` · `/{key_id}` | Rotate with an overlap window / revoke immediately | `iam.serviceaccounts.manage` |
| GET | `/api/v1/iam/service-accounts/{id}/usage` | Daily series, top paths, recent denials | `iam.serviceaccounts.read` |
| POST | `/api/v1/iam/service-accounts/{id}/verify` | Step-up-protected self-test: does this key still authenticate | `iam.serviceaccounts.manage` |

Key-bearing responses are `no-store`; issuing a key requires a fresh step-up (the same rule REQ-006 applies to MFA reset and key issuance). Every route carries `guards::require("<key>")` and answers `404` for a foreign organization's account.

### Data model

```text
service_accounts        (id, organization_id references organizations (id) on delete cascade,
                         name, purpose text not null default '', owner_id uuid references users (id) on delete set null,
                         status text not null default 'active' check (status in ('active','disabled')),
                         archived_at timestamptz, expires_at timestamptz, last_used_at timestamptz,
                         created_by uuid, created_at, updated_at)
service_account_keys    (id, service_account_id references service_accounts (id) on delete cascade,
                         name text not null, prefix text not null, secret_hash bytea not null,
                         permission_subset text[], ip_allowlist cidr[], expires_at timestamptz,
                         rotating_until timestamptz, rotated_from_key_id uuid references service_account_keys (id) on delete set null,
                         last_used_at timestamptz, use_count bigint not null default 0,
                         revoked_at timestamptz, revoked_by uuid, created_by uuid, created_at)
service_account_usage   (key_id references service_account_keys (id) on delete cascade, day date,
                         requests integer not null default 0, denials integer not null default 0,
                         top_path text, primary key (key_id, day))
```

Indexes: unique `(organization_id, lower(name))` on service accounts; unique `prefix` on keys (the only lookup path from a request); `service_account_keys_sa_idx (service_account_id) where revoked_at is null`; `service_account_keys_expires_idx (expires_at) where revoked_at is null`; `service_accounts_org_status_idx (organization_id, status) where archived_at is null`; `service_accounts_last_used_idx (last_used_at)` for the stale sweep.

Migration `database/migrations/0018_service_accounts_and_api_keys.sql`: creates only what the REQ-006 migration did not (matching its table and column names exactly if it shipped first — one table per concept), comments in the `0002` style, seeds nothing. Grants are data, not schema — no role is attached by the migration.

### Events

- Emitted: `iam.serviceaccount_created`, `iam.serviceaccount_updated`, `iam.serviceaccount_disabled`, `iam.serviceaccount_key_issued`, `iam.serviceaccount_key_rotated`, `iam.serviceaccount_key_revoked`, `iam.serviceaccount_key_expiring` (14/7/1 days, from the scheduler), `iam.serviceaccount_key_stale` (90 days unused), `iam.serviceaccount_auth_denied`.
- Consumed: `user.deactivated` flags owned accounts as `owner_missing` and emits a notification request; `iam.role_permissions_changed` (REQ-067) invalidates the cached effective set of every affected service account; `iam.provisioning_synced` never touches these tables.
- Payloads carry ids, the key prefix (never the secret or its hash), the window and the owner. `iam.serviceaccount_auth_denied` is rate-limited and sampled before webhooks (a revoked key in a retry loop would otherwise flood subscribers); the security centre (REQ-012) subscribes to the key lifecycle events.

### Acceptance criteria

- [ ] `0018_service_accounts_and_api_keys.sql` applies on a fresh and on a populated database and coexists with the REQ-006 tables; `cargo test --workspace` is green.
- [ ] Creating a service account with a role at organization scope lets a `Bearer` request succeed on a route that role allows and answer `403` on one it does not, with the decision source in the body.
- [ ] A key secret appears once: the creation response contains it, a second read of the key resource answers `410`, and the database stores only the prefix and a hash (asserted by a test that greps the test schema).
- [ ] A revoked key fails on the next request with `401`; a disabled account fails `401` even with an otherwise valid key.
- [ ] An expired key (`expires_at` in the past) fails `401`; a key inside its rotation overlap window works while the new key also works, and the old key stops exactly at `rotating_until`.
- [ ] A key's `permission_subset` narrows and never widens: a key whose subset lacks `content.pages.delete` answers `403` on delete even though the account's role allows it.
- [ ] An IP outside the key's allowlist is refused before any permission check, and the denial reason is recorded.
- [ ] A service-account Authorization header cannot create, refresh or revoke a session; sign-in, password, MFA and `users.impersonate` routes answer `403` for this subject.
- [ ] Binding an interactive-only key (`users.impersonate`, `iam.providers.*`, `iam.serviceaccounts.*`) to a service account is refused with a field-level error naming the key.
- [ ] Every key-authenticated request — allowed or denied — writes an audit entry with key id, method, path, decision and client IP; no secret, prefix-only or hash appears in any audit row or log line (checked by a test over captured output).
- [ ] `last_used_at` and the daily rollup update after a burst of requests without writing one usage row per request.
- [ ] The stale and expiry sweeps emit their events exactly once per day per key (idempotent job, proven by running it twice).
- [ ] Issuing or rotating a key requires step-up; without it the API answers `403` with the step-up hint and the UI opens the step-up dialog.
- [ ] Owner deactivation marks the account `owner_missing`, it appears in the attention band, and disabling the owner does not break the machine identity's existing keys.
- [ ] Templates pre-fill name, purpose and suggested grants and never grant silently (a test asserts no binding exists after picking a template until the form is submitted).
- [ ] Every screen has empty, loading and error states, the keys table renders 200 keys without layout breakage, and the QA pass reports zero high findings.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/settings/iam/service-accounts`, `/settings/iam/service-accounts/{id}` and its `keys` and `usage` tabs (desktop) plus the list (mobile). The script must create an account from the `CI/CD` template, assert nothing is granted before submit, attach a role at site scope, create a key, assert the secret dialog requires the acknowledgement checkbox, copy the value and close it, then re-open the key and assert the rotation/`410` behaviour is explained. It must rotate the key and assert the overlap window text, revoke it and assert the status chip, open the usage tab (empty state before any call is a valid assertion), and submit an invalid CIDR in the allowlist field to assert the field-level error. Screenshots `page-iam-service-accounts`, `page-iam-service-account-detail`, `page-iam-service-account-key`, `mobile-iam-service-accounts`; the visual check looks for an unmissable one-time-secret warning, readable status chips at AA contrast and no horizontal scroll on mobile.

### Slices

1. **Registry, grants and templates.** Migration, CRUD with archive and owner rules, binding and explicit-deny support, effective-permissions view, create wizard, list with the attention band, events and audit entries. *Done when:* acceptance 1–2, 9 and 15 pass and the walkthrough creates an account with a scoped grant.
2. **Keys and the authentication path.** Key issue/verify/revoke with prefix + hash storage, rotation with overlap, expiry and IP allowlist, per-key subset, Bearer resolution in `apps/api/src/auth.rs`, non-interactive invariant, audit and usage rollups, secret dialog. *Done when:* acceptance 3–8, 10–11 and 13 pass, and a revoked key is rejected on the next request in an integration test.
3. **Visibility, reminders and hardening.** Usage tab, stale/expiry sweeps with idempotence, notification wiring, bulk actions, performance pass on the request path, documentation of the template set. *Done when:* acceptance 12 and 14 pass and the sweeps run twice in CI without duplicate events.

### Risks / notes

- **Secret handling is the whole request.** Hash at rest with the same helper sessions use (`crates/identity::sessions`), one-time display, no logging; an intermediate buffer must never be written to stdout, and the review step of the wizard says so out loud. Do not copy the plaintext `ai_providers.api_key` column pattern for anything new — provider credentials belong to REQ-037 by reference.
- **The prefix is the only lookup key.** It must be unique, non-sequential and long enough to avoid enumeration; verification compares the hash in constant time and the failure path is indistinguishable between "unknown prefix" and "wrong secret" in the response, with the distinction only in the audit row.
- **Non-interactive invariant needs a test, not a comment.** A single forgotten route that accepts a key where a session is required would let a machine identity change a password or enrol a factor; keep the refusal in the guard, not per handler.
- **Audit volume.** One row per authenticated request is the cost of the guarantee; keep the write inside the request's transaction so a decision is never observable without its record, and aggregate counters separately rather than scanning audit on every page load.
- **Rotation is where operators make mistakes.** Show the exact overlap end time before confirming, never invalidate silently, and refuse rotation of an already revoked key with `409`.
- **Expiry and clock.** Expiry is evaluated server-side against one clock; a key at the boundary is refused rather than accepted twice, and the reminder job runs idempotently so a restart does not double the notifications.
