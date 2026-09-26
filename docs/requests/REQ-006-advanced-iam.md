# REQ-006 — Advanced IAM

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/auth`, `crates/permissions`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Beyond LDAP/AD, the identity stack adds:

- OIDC
- OAuth2
- SAML
- WebAuthn
- Passkeys
- MFA
- SCIM
- Session management
- Device management
- IP restrictions
- Login policies
- Password policies
- RBAC
- ABAC
- Custom roles

Example — per-resource permission matrix:

```text
Marketing Manager

Pages:
  read ✓
  create ✓
  update ✓
  delete ✗

Users:
  read ✓
  manage ✗

Billing:
  ✗
```

## Notes

- Builds on the identity/authorization vision in docs/01-VISION.md §2 and
  docs/02-ARCHITECTURE.md (Enterprise side).
- Full design: [`docs/07-IAM.md`](../07-IAM.md) — custom roles, granular permissions,
  hierarchy + inheritance, allow/deny precedence, scopes, ABAC, policy builder, permission
  simulator, service accounts, temporary roles.

## Implementation spec

> **Where:** `crates/identity` (accounts, sessions, devices, MFA factors, sign-in providers) · `crates/permissions` (roles, bindings, groups, service accounts) · **new** `crates/policy-engine` (ABAC) and `crates/authorization` (one decision path + simulator) · **Migration:** `database/migrations/0011_iam_advanced.sql` (next free slot at build time) · **Admin routes:** `/settings/iam/*` · **Permission family:** `iam.*`, `users.*`, `audit.read` · **Depends on:** `crates/audit`, `crates/events`, `crates/notifications` (REQ-021), `crates/ai-hub` (agents and service accounts become subjects of this same binding model, docs/07 §14).

### Scope (in / out)

**In**

- **Roles to the full docs/07 model:** role CRUD, priority (0–1000), single-parent inheritance with cycle detection (depth ≤ 8), explicit allow/deny entries, effective resolution with precedence `explicit deny > explicit allow > inherited allow > default deny`, versioned history with diffs.
- **Scopes:** bindings at `global`, `organization`, `site`, `department`, `module`, `resource`; resource bindings carry `resource_type`/`resource_id` (a path glob such as `/blog/*` on a site); `expires_at` gives temporary roles — an expired binding stops counting without being deleted.
- **Subjects:** users, groups (teams) and service accounts share one binding table, so group membership and machine identities grant exactly the way a user binding does.
- **ABAC + policy builder:** `crates/policy-engine` evaluates a condition tree (`==`, `!=`, `>`, `<`, `in`, `starts_with`, `contains`; `and`/`or`/`not`) with an effect, target permission keys and a priority, after RBAC — deny wins and a missing attribute compares as `null`; the builder offers a visual editor, a JSON view and a dry run against sample attributes, and versions every save.
- **Permission simulator:** `(subject, action, resource)` → decision plus the explanation chain (binding → role → permission source → scope → policy) and, on denial, the winning deny.
- **Sessions & devices:** session list and revocation per user, idle and absolute lifetime, concurrent-session cap, a known-device registry with a trust window and a new-device notice event.
- **Login, password, IP and MFA policy:** one document per organization — password length/classes/history/expiry, lockout thresholds, IP allow/deny CIDR lists, session lifetime, device trust window and MFA requirement; TOTP, WebAuthn/passkeys and single-use recovery codes with enrolment, admin reset and step-up for dangerous operations.
- **Enterprise sign-in + provisioning:** OIDC, generic OAuth2 and SAML 2.0 providers per organization with JIT provisioning and claim → role mapping (local sign-in stays available), plus SCIM 2.0 user/group provisioning with hashed per-organization tokens and a sync log.
- **Service accounts and approvals:** machine identities with prefix + hashed keys, expiry, rotation and last-used tracking; a user can request a permission for a window and an approver grants or rejects it as a time-boxed binding.
- **Permission safety invariants:** at least one Owner and one Administrator per organization, no self-lockout, no removing the last owner binding — refused with an error naming the invariant.

**Out (tracked elsewhere)**

- LDAP/AD directory sync → REQ-015; security dashboards, anomaly scoring and alerts → REQ-012 (this request only emits the events they consume); advanced audit retention → REQ-039; notification rendering → REQ-021; AI agent permission sets → REQ-001 / REQ-047; organization hierarchy and tenant lifecycle → REQ-005.

### Screens (UI)

Nav: **Overview · Users · Roles · Groups · Service accounts · Policies · Simulator · Authentication · Security · Sessions · Devices · Approvals · Provisioning**.

| Route | Screen |
|---|---|
| `/settings/iam` | Overview: counts, bindings expiring in 7 days, failed sign-ins today, blocked requests, recent security events |
| `/settings/iam/users` | User list |
| `/settings/iam/users/{id}` | User detail — tabs `Profile`, `Roles & bindings`, `Effective permissions`, `Sessions`, `Devices`, `Audit` |
| `/settings/iam/roles` | Role list (platform + custom) with allowed/denied counts |
| `/settings/iam/roles/{id}` | Role detail — tabs `Permissions` (matrix), `Members`, `Inherited by`, `History` |
| `/settings/iam/groups` | Groups (teams): members and attached roles |
| `/settings/iam/service-accounts` | Machine identities and their keys |
| `/settings/iam/policies` | ABAC policy list + builder |
| `/settings/iam/simulator` | Permission simulator |
| `/settings/iam/authentication` | Sign-in providers (local, OIDC, OAuth2, SAML) |
| `/settings/iam/security` | Password / lockout / IP / session / device policy tabs |
| `/settings/iam/sessions` | Active sessions |
| `/settings/iam/devices` | Known devices |
| `/settings/iam/approvals` | Permission request inbox |
| `/settings/iam/provisioning` | SCIM tokens + sync log |

**User list** — columns `Name` (avatar initials, link), `E-mail`, `Status`, `Organization`, `Roles` (chips, `+N`), `MFA`, `Last sign-in`, `Updated`; filters: search (250 ms debounce), status, organization, role, MFA state, sign-in range; bulk actions: assign role (scope picker), remove binding, require MFA, suspend/resume, sign out everywhere, export; row actions: open, edit drawer, reset MFA, copy invite link.

**Role matrix** — a category accordion (content, media, users, plugins, deployment, iam, tenancy, webhooks, events, ai) beside one row per permission key with a tri-state cell (`allow` / `deny` / `inherit`), header counts (`allowed N · denied M · inherited K`), grant-all/clear-category, search, a sticky `Save`/`Discard` footer and a diff preview of what will change. Validation refuses an unknown key, a duplicate entry, a priority outside `0–1000` and an inheritance cycle.

**Policy builder + simulator** — condition rows (attribute → operator → value, `AND`/`OR` grouping, `NOT`), a `THEN` block (effect, target permissions, priority, enabled) and a sample-attributes panel whose `Test` highlights matched conditions; attributes come from `users.attributes` plus request attributes (`resource.site_id`, `resource.path`, `action`). The simulator takes subject / action / resource (site plus optional page path), renders `ALLOWED` or `DENIED` with the decision path, and offers `Copy as test case`.

**Security, sessions, devices** — five policy tabs with range-checked fields (password `8–128`, expiry `0–730` days where `0` = never, history `0–24`; lockout `3–50` attempts and `1–1440` minutes; allow/deny CIDR textareas with per-line validation where deny wins; session idle `5–10080` minutes, absolute `1–365` days, concurrent `1–100`; device trust `0–365` days) and an audit entry with a before/after diff on save. Session columns `User`, `IP`, `Device`, `Auth methods`, `Started`, `Last seen`, `Expires`, `Actions` with bulk revoke; device columns `User`, `Label`, `Platform`, `First seen`, `Last seen`, `Trusted until`, `Actions`.

**States, keyboard, mobile** — skeletons on first paint, a real empty state with the primary action, an error state with retry; `/` focuses search, `j`/`k` move row focus, `enter` opens, `e` edits, `?` shows the shortcut sheet, `Esc` closes drawers, `space` cycles a focused matrix cell; below `lg` tables become cards, the matrix becomes per-category accordions with the same tri-state control, and forms go single-column with a sticky save bar.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET, POST | `/api/v1/iam/roles` | List roles with entry counts / create a custom role | `iam.roles.read` / `iam.roles.manage` |
| GET, PATCH, DELETE | `/api/v1/iam/roles/{id}` | Detail with inherited chain / update priority + parent / delete while unbound | `iam.roles.read` / `iam.roles.manage` |
| PUT, POST | `/api/v1/iam/roles/{id}/permissions` · `/preview` | Replace the allow/deny set / diff preview before saving | `iam.roles.manage` |
| POST, GET | `/api/v1/iam/roles/{id}/duplicate` · `/roles/{id}/versions` | Clone a role / read version history with diffs | `iam.roles.manage` / `iam.roles.read` |
| GET, POST | `/api/v1/iam/users` | User list / create or invite | `users.read` / `users.create` |
| GET, PATCH | `/api/v1/iam/users/{id}` | Profile, bindings, factors / update profile, attributes, status | `users.read` / `users.update` |
| POST, GET | `/api/v1/iam/users/{id}/reset-mfa` · `/effective-permissions` | Clear factors after step-up / resolved permission set with sources | `users.update` / `users.read` |
| GET, POST, DELETE | `/api/v1/iam/bindings` · `/bindings/{id}` | List, create or revoke bindings (subject, scope, window) | `iam.bindings.read` / `iam.bindings.manage` |
| GET, POST | `/api/v1/iam/groups` | Groups with counts / create a group | `iam.groups.read` / `iam.groups.manage` |
| PATCH, DELETE, PUT | `/api/v1/iam/groups/{id}` · `/members` | Update, delete, replace membership | `iam.groups.manage` |
| GET, POST | `/api/v1/iam/service-accounts` | Machine identities / create one | `iam.serviceaccounts.read` / `iam.serviceaccounts.manage` |
| POST, DELETE | `/api/v1/iam/service-accounts/{id}/keys` | Issue a key (secret once) / revoke one | `iam.serviceaccounts.manage` |
| GET, POST | `/api/v1/iam/policies` | ABAC policy list / create | `iam.policies.read` / `iam.policies.manage` |
| PUT, DELETE, POST | `/api/v1/iam/policies/{id}` · `/test` | Update (new version), delete, dry run | `iam.policies.manage` |
| POST | `/api/v1/iam/simulations` | Decision plus explanation | `iam.simulate` |
| GET, PUT | `/api/v1/iam/security-policies` | Read / update password, lockout, IP, session, device policy | `iam.security.read` / `iam.security.manage` |
| GET, DELETE, POST | `/api/v1/iam/sessions` · `/sessions/{id}` · `/users/{id}/sign-out-all` | Active sessions, revoke one, revoke all of a user | `iam.sessions.read` / `iam.sessions.revoke` |
| GET, POST, DELETE | `/api/v1/iam/devices` · `/devices/{id}` | Known devices / set trust window / forget | `iam.devices.read` / `iam.devices.manage` |
| GET, POST, PATCH | `/api/v1/iam/providers` · `/providers/{id}/test` | Sign-in providers: list, connect, update, discovery test (secrets by reference) | `iam.providers.read` / `iam.providers.manage` |
| GET, POST | `/api/v1/iam/approvals` · `/approvals/{id}/decide` | Request inbox / approve with a window or reject | `iam.approvals.read` / `iam.approvals.decide` |
| POST | `/api/v1/iam/requests` | Request a permission for a window | signed-in session |
| GET, POST | `/api/v1/iam/provisioning/tokens` · `/provisioning/log` | SCIM tokens (secret shown once) and the sync log | `iam.provisioning.manage` |
| GET, POST | `/api/v1/auth/sso/{provider}/start` · `/callback` | OIDC/OAuth2/SAML sign-in | public sign-in route |
| POST | `/api/v1/auth/mfa/verify` | Second factor after password | public sign-in route |
| POST, GET, DELETE | `/api/v1/auth/webauthn/...` · `/api/v1/auth/sessions/{id}` | Passkey enrolment/listing/removal and own sessions | signed-in session |
| GET, POST, PATCH, DELETE | `/api/v1/scim/v2/Users` · `/Users/{id}` · `/Groups` | SCIM 2.0 provisioning | provisioning token |

### Data model

Extensions (additive; constraint changes are expand-then-contract per docs/05-VERSIONING.md):

```text
users         + mfa_enforced boolean not null default false, last_sign_in_at timestamptz,
                failed_sign_in_count integer not null default 0, locked_until timestamptz,
                attributes jsonb not null default '{}'                    -- ABAC subject attributes
sessions      + device_id uuid, auth_methods text[] not null default '{}',
                absolute_expires_at timestamptz, revoked_by uuid, revoke_reason text
role_bindings + subject_type text not null default 'user' check (subject_type in ('user','group','service_account')),
                subject_id uuid not null, resource_type text, resource_id text   -- user_id kept for one release
```

New tables: `user_devices` (user, fingerprint hash, label, platform, browser, first/last seen, trust window, revoked) · `mfa_factors` (user, kind `totp`/`webauthn`/`recovery`, label, secret ciphertext, credential id, public key, sign count, transports, confirmed/last used) · `mfa_recovery_codes` (code hash, used at) · `groups` + `group_members` · `service_accounts` + `service_account_keys` (name, prefix, secret hash, expiry, last used, revoked) · `policies` (effect, priority, `conditions jsonb`, `target_permissions text[]`, enabled, version) + `policy_versions` · `role_versions` · `security_policies` (one row per organization: password/lockout/session/device fields, `ip_allowlist cidr[]`, `ip_denylist cidr[]`) · `sign_in_attempts` (email, user, ip, user agent, outcome, time) · `auth_providers` (`config jsonb`, `secret_ref`, scopes, group claim, default role, JIT flag, enabled) · `permission_requests` (permission key, resource, justification, status, grant minutes, resulting binding) · `provisioning_tokens` + `provisioning_log`.

Indexes: `role_bindings_subject_idx (subject_type, subject_id) where revoked_at is null`, `role_bindings_expires_idx (expires_at) where revoked_at is null`, `policies_org_enabled_idx (organization_id, enabled, priority)`, `sign_in_attempts_email_idx (lower(email), created_at desc)`, `sign_in_attempts_ip_idx (ip, created_at desc)`, `user_devices_user_idx (user_id) where revoked_at is null`, GIN on `users.attributes` and `policies.target_permissions`, unique `(organization_id, lower(name))` on groups and service accounts, unique `(organization_id, slug)` on providers.

Migration `database/migrations/0011_iam_advanced.sql` — append-only and commented in the `0002` style; it seeds one `security_policies` row per existing organization with the conservative defaults above and backfills `role_bindings.subject_id` from `user_id`.

### Events

- Emitted: `iam.role_created`, `iam.role_updated`, `iam.role_permissions_changed`, `iam.binding_created`, `iam.binding_revoked`, `iam.user_created`, `iam.user_disabled`, `iam.session_revoked`, `iam.signin_failed`, `iam.account_locked`, `iam.mfa_enrolled`, `iam.policy_changed`, `iam.policy_denied`, `iam.serviceaccount_key_issued`, `iam.approval_requested`, `iam.approval_decided`, `iam.provisioning_synced`.
- Payloads carry ids, the changed field list and the decision source — never a secret, a hash or a rendered policy document; `iam.policy_denied` fires only for state-changing `/api/v1` calls. Consumed: `user.created` provisions the default member binding and the personal group; `iam.approval_decided` activates the time-boxed grant. Webhook relevance: the security centre (REQ-012) subscribes to `iam.signin_failed`, `iam.account_locked` and `iam.policy_changed`, and automations (REQ-003) may trigger on approvals — keep the subscription list curated, since an endpoint accepts at most 32 event names.

### Acceptance criteria

- [ ] `0011_iam_advanced.sql` applies on a fresh and on a populated database; `cargo test --workspace` is green.
- [ ] Precedence is proven: an explicit deny in one role beats an explicit allow in another; inherited allows apply unless denied; no binding means default deny.
- [ ] An inheritance cycle (A → B → A) and self-inheritance are refused with a field-level error.
- [ ] Matrix save is atomic: an unknown key, a duplicate entry or a stale version fails the whole save with a diff of what was rejected.
- [ ] A resource-scoped binding (`site` + `/blog/*`) allows a matching path and denies `/legal/…` with the decision source in the 403; a past `expires_at` stops counting without deleting the row (the members tab shows it as expired).
- [ ] Group membership grants and revokes: adding a user to a group with an attached role changes the effective set on the next request; removal reverses it.
- [ ] A service-account key authenticates a `/api/v1` request over Bearer and cannot start an interactive sign-in session.
- [ ] The simulator’s verdict equals the guard’s verdict across a test matrix of ≥ 100 (subject, action, resource) cases.
- [ ] Safety invariants hold: removing the last owner binding, or the caller’s own last privileged binding, is refused with a message naming the invariant.
- [ ] A revoked session is rejected on the next request and `sign-out-all` clears every session (one event each); idle timeout, absolute lifetime and the concurrent cap come from the policy row, never from constants.
- [ ] Lockout works per account and per IP with outcomes recorded in `sign_in_attempts`; a denied IP is refused before any password check; TOTP and a passkey both enrol and verify; step-up is demanded for MFA reset and key issuance; a recovery code works exactly once.
- [ ] OIDC and SAML sign-in complete against a test provider with JIT provisioning and the mapped role; a SCIM create → update → deactivate round trip appears in the sync log.
- [ ] An approved request grants the permission only inside its window and expires on its own; every role, binding, policy, session, device and approval change writes an audit entry; all routes answer 401/403/200 as documented; every screen has empty, loading and error states with zero high findings in the QA pass.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/settings/iam`, `/settings/iam/users`, `/settings/iam/roles`, `/settings/iam/groups`, `/settings/iam/service-accounts`, `/settings/iam/policies`, `/settings/iam/simulator`, `/settings/iam/authentication`, `/settings/iam/security`, `/settings/iam/sessions`, `/settings/iam/devices`, `/settings/iam/approvals` (desktop) and `/settings/iam/users`, `/settings/iam/roles` (mobile). The script must create a role, cycle three matrix cells (allow → deny → inherit), save and reopen it, run two simulator queries (one expected `ALLOWED`, one `DENIED`) and read the explanation, open the policy builder and run `Test`, revoke one session, open then cancel the MFA enrolment dialog, submit an invalid CIDR and assert the field-level error, and create a service account to see the key shown once.

What the visual check should see: a matrix with a sticky category header, tri-state cells at AA contrast, platform and custom roles visually distinct, no clipped policy JSON, an unmistakable simulator verdict card with a readable source list, and a clean mobile layout with cards instead of tables — screenshots `page-iam-users`, `page-iam-roles`, `page-iam-role-matrix`, `page-iam-simulator`, `page-iam-security`, `mobile-iam-users`.

### Slices

1. **Role depth.** Migration, role CRUD/duplicate/delete, matrix editor with tri-state and diff preview, inheritance + precedence resolution, role versions, catalogue additions, tests. Done when the matrix saves atomically, precedence and cycle tests pass, and the history tab shows a diff.
2. **Subjects, scopes, simulator.** Users screen, bindings at every scope level with expiry, groups, service accounts + keys, effective permissions and the RBAC part of the simulator. Done when the simulator agrees with the API guard for every catalogue key across two subjects and a resource-scoped case.
3. **Sessions, devices, MFA, security policy.** Session/device screens with revocation, idle/absolute lifetime, lockout, IP lists, TOTP + passkey enrolment and step-up. Done when a revoked session is rejected on the next request, lockout triggers at the configured threshold, and both factors enrol and verify in the QA stack.
4. **Enterprise sign-in, provisioning, ABAC, approvals.** OIDC/OAuth2/SAML providers, SCIM, policy engine + builder + test, permission requests and grants, safety invariants. Done when SSO sign-in completes against a test provider, a SCIM round trip provisions a user, an ABAC policy flips a decision, and the last-owner invariant blocks self-lockout.

### Risks / notes

- **Migration number** is “next free slot at build time”; the file stays additive. The `role_bindings` subject/scope change is expand-then-contract — keep writing `user_id` until a later release drops it.
- **One decision path:** the guard, the list filters and the simulator must call the same `crates/authorization` function; a second implementation is a release blocker because it drifts silently.
- **Precedence must stay SQL-expressible** for list filtering — benchmark resolution against a seeded 5k-binding organization and keep it inside a request budget.
- **Secrets by reference only:** provider credentials live behind `secret_ref` (environment or secret store, REQ-037), MFA secrets are stored encrypted, and codes are never logged.
- **Provisioning tokens** are prefix + hash and rotatable; the sync log drops personal data on retention, and deactivation (not deletion) is the SCIM default.
- **SSO, passkeys and event volume:** document the local `localhost` exception so QA can exercise WebAuthn (a test that silently skips is not evidence), and sample or aggregate `iam.policy_denied`/`iam.signin_failed` before they reach webhooks.
