# REQ-006 — Advanced IAM

> **Status:** in-progress — slices 1–3b, 4a and 4b-1 shipped (role depth; subjects/scopes/simulator; security policy / sessions / devices / TOTP / WebAuthn passkeys; ABAC policies + the builder + the safety invariants; permission requests + approvals with their time-boxed grants, and SCIM 2.0 provisioning with its sync log); slice 4b-2 in progress — the enterprise sign-in **core** is shipped and pushed (`a40903a`: OIDC discovery + PKCE + RS256 verification, SAML 2.0 with both halves of the XML-signature binding, single-use challenges, claim → role mapping; 103 identity tests green) and the API, the `/settings/iam/authentication` screen, JIT provisioning and the QA pass are next
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

- [x] `0011_iam_advanced.sql` applies on a fresh and on a populated database; `cargo test --workspace` is green. *(slice 1: applied to a scratch database and to the populated dev database — the `subject_id` backfill and the policy seed were read back; see BUILD-LOG 2026-09-27.)*
- [x] Precedence is proven: an explicit deny in one role beats an explicit allow in another; inherited allows apply unless denied; no binding means default deny. *(slice 1: `apps/api/tests/iam.rs::role_depth_lifecycle_is_proven_end_to_end` — an unbound account is refused, the allowing binding admits it, the denying binding takes it away again; the resolver unit tests cover inheritance.)*
- [x] An inheritance cycle (A → B → A) and self-inheritance are refused with a field-level error. *(slice 1: `409 role_inheritance_cycle`, message names `inherits_role_id`; the depth limit (8) answers `400 role_inheritance_depth`.)*
- [x] Matrix save is atomic: an unknown key, a duplicate entry or a stale version fails the whole save with a diff of what was rejected. *(slice 1: `400 invalid_entries` naming the rejected keys, `409 role_version_conflict`, and the set on disk is provably unchanged after a refusal; a successful save returns the added/changed/removed diff and a version number.)*
- [x] A resource-scoped binding (`site` + `/blog/*`) allows a matching path and denies `/legal/…` with the decision source in the 403; a past `expires_at` stops counting without deleting the row (the members tab shows it as expired). *(slice 2: a `resource` binding on `/blog/*` allows `/blog/hello-world` and refuses `/legal/terms` — the simulator reports the binding as `out_of_scope` and the guard's `403 permission_denied` carries `details.{permission,reason,source,context}`; an expired binding answers `expired`, grants nothing and stays listed in the members tab.)*
- [x] Group membership grants and revokes: adding a user to a group with an attached role changes the effective set on the next request; removal reverses it. *(slice 2: the walk creates a group, binds the Member role to it, sees the permission arrive for the member over HTTP, clears the membership and sees it leave.)*
- [x] A service-account key authenticates a `/api/v1` request over Bearer and cannot start an interactive sign-in session. *(slice 2: `omsa_…` key → `200` on `/iam/simulations` with the machine as the default subject; `401 unauthorized` on a session-only route, `401` and no cookie on `/auth/login`, and `401 invalid_machine_key` the moment the key is revoked.)*
- [x] The simulator’s verdict equals the guard’s verdict across a test matrix of ≥ 100 (subject, action, resource) cases. *(slice 2: three (subject, context) cases — a user at organization scope, the same user on a resource path and a service account — against the whole catalogue: 216 comparisons, each next to the guard's own `authorize_subject`.)*
- [x] Safety invariants hold: removing the last owner binding, or the caller’s own last privileged binding, is refused with a message naming the invariant. *(slice 4a: `apps/api/tests/iam_policy.rs::abac_policies_and_the_safety_invariants_are_proven_end_to_end` — the Owner’s own platform binding answers `409 self_lockout` naming “your own last”, the organization’s last organization-scoped owner binding answers `409 last_owner_binding` naming “at least one”, a refused change leaves the row live, and an ordinary revocation still goes through once a second privileged binding exists.)*
- [x] An ABAC policy flips a decision on the one decision path: a `deny` policy takes away what RBAC granted, an `allow` policy grants what RBAC never gave, priority decides between them, a disabled policy decides nothing, and the simulator’s verdict equals the guard’s in every one of those states; every save stores a version, and the dry run evaluates a condition tree over sample attributes before anything is saved. *(slice 4a: the same walk drives all five states over the real router and reads the three recorded versions back; `crates/policy-engine` pins the operators, the null semantics, the deny-wins tie and the wildcard targets; the walk also proves the dry run’s leaf-by-leaf trace and the `invalid_policy` refusals. The walkthrough drives the builder in the browser — pass `iam-policies`.)*
- [x] A revoked session is rejected on the next request and `sign-out-all` clears every session (one event each); idle timeout, absolute lifetime and the concurrent cap come from the policy row, never from constants. *(slice 3a: the same walk revokes one session and the next request answers `401`, `sign-out-all` ends both live sessions, the same untouched row is refused at a five-minute idle window and accepted once the policy says two hours (so it is the policy, not a constant), and the concurrent cap of two retires the oldest with `revoke_reason = 'concurrent_cap'`.)*
- [x] Lockout works per account and per IP with outcomes recorded in `sign_in_attempts`; a denied IP is refused before any password check; step-up is demanded for MFA reset and key issuance; a recovery code works exactly once. *(slice 3a: `apps/api/tests/iam.rs::sessions_devices_mfa_and_the_security_policy_are_proven_end_to_end` — three failures lock the account at the threshold the policy names and the fourth answer is `account_locked`; the correct password is refused while locked; a denied address answers `address_blocked` for the correct password as well, an allowlist refuses an address outside it, and the fourth failure from one address is refused by the address count; TOTP enrols, confirmation issues ten recovery codes, a sign-in answers a challenge instead of a cookie, and a recovery code works exactly once; `reset-mfa` and key issuance both answer `403 step_up_required` until the caller proves identity again.)*
- [x] TOTP and a **passkey** both enrol and verify. *(slice 3a ships TOTP; slice 3b ships WebAuthn: `crates/identity/src/webauthn/` parses the client data, the authenticator data and the CBOR attestation object, extracts the COSE key for **ES256** and **EdDSA**, verifies the signature over `authenticatorData || SHA-256(clientDataJSON)`, refuses a counter that does not move forward, and accepts the documented loopback origin so the QA stack can run a real ceremony. Proof: `cargo test --workspace` — the ceremony unit tests (both families plus every refusal) and `apps/api/tests/webauthn.rs::a_passkey_enrols_and_signs_in_end_to_end`, which registers a credential with a software authenticator, signs in with an assertion, proves the session carries `webauthn` in its auth methods, refuses a replayed counter and a foreign origin, demands a step-up to remove the factor and ends password-only again. Browser proof: the `iam-passkeys` pass of `scripts/qa/walkthrough.cjs` drives the panel with a Chrome virtual authenticator.)*
- [ ] OIDC and SAML sign-in complete against a test provider with JIT provisioning and the mapped role. *(part 1 shipped in `a40903a`: the protocol layer is done and proved against real cryptography — RS256 verification against a generated 2048-bit key, the SAML signature checked in **both** directions (the declared `DigestValue` over the enveloped transform, so a tampered claim is refused, plus the RSA signature over `SignedInfo`), issuer/audience/window refusals, entity-declaration and unsigned responses refused, and the claim → role mapping read from arrays, space-delimited strings and nested arrays. The API routes, the panel screen and the JIT walk are next; the SCIM half was proven in 4b-1 with `apps/api/tests/scim.rs::a_scim_round_trip_provisions_and_logs` and the `iam-provisioning` pass.)* *(the SCIM half is proven — a create → patch → deactivate round trip over the real router, recorded in the sync log, `apps/api/tests/scim.rs::a_scim_round_trip_provisions_and_logs`, and driven in the browser by the `iam-provisioning` pass; SSO arrives with slice 4b-2.)*
- [x] An approved request grants the permission only inside its window and expires on its own; every role, binding, policy, session, device and approval change writes an audit entry; all routes answer 401/403/200 as documented; every screen has empty, loading and error states with zero high findings in the QA pass. *(slice 4b-1: `apps/api/tests/iam_approvals.rs::an_approved_request_grants_only_inside_its_window` moves a granted window into the past and watches the permission leave with nobody acting; the member cannot read the inbox, cannot decide and cannot read the user list after a refusal; the audit trail carries `iam.approval.requested` / `approved` / `rejected`; the walkthrough drives `iam-approvals` and `iam-provisioning` on desktop and mobile.)*

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/settings/iam`, `/settings/iam/users`, `/settings/iam/roles`, `/settings/iam/groups`, `/settings/iam/service-accounts`, `/settings/iam/policies`, `/settings/iam/simulator`, `/settings/iam/authentication`, `/settings/iam/security`, `/settings/iam/sessions`, `/settings/iam/devices`, `/settings/iam/approvals` (desktop) and `/settings/iam/users`, `/settings/iam/roles` (mobile). The script must create a role, cycle three matrix cells (allow → deny → inherit), save and reopen it, run two simulator queries (one expected `ALLOWED`, one `DENIED`) and read the explanation, open the policy builder and run `Test`, revoke one session, open then cancel the MFA enrolment dialog, submit an invalid CIDR and assert the field-level error, and create a service account to see the key shown once.

What the visual check should see: a matrix with a sticky category header, tri-state cells at AA contrast, platform and custom roles visually distinct, no clipped policy JSON, an unmistakable simulator verdict card with a readable source list, and a clean mobile layout with cards instead of tables — screenshots `page-iam-users`, `page-iam-roles`, `page-iam-role-matrix`, `page-iam-simulator`, `page-iam-security`, `mobile-iam-users`.

### Slices

1. **Role depth.** Migration, role CRUD/duplicate/delete, matrix editor with tri-state and diff preview, inheritance + precedence resolution, role versions, catalogue additions, tests. Done when the matrix saves atomically, precedence and cycle tests pass, and the history tab shows a diff.
2. **Subjects, scopes, simulator.** Users screen, bindings at every scope level with expiry, groups, service accounts + keys, effective permissions and the RBAC part of the simulator. Done when the simulator agrees with the API guard for every catalogue key across two subjects and a resource-scoped case.
3. **Sessions, devices, MFA, security policy.** Session/device screens with revocation, idle/absolute lifetime, lockout, IP lists, TOTP + passkey enrolment and step-up. Done when a revoked session is rejected on the next request, lockout triggers at the configured threshold, and both factors enrol and verify in the QA stack.
4. **Enterprise sign-in, provisioning, ABAC, approvals.** Split in three. **4a (shipped)** — the ABAC policy engine with its builder, dry run and versions, plus the permission safety invariants. **4b-1 (shipped)** — permission requests and approvals as time-boxed bindings, and SCIM 2.0 provisioning with its sync log. **4b-2 (pending)** — OIDC/OAuth2/SAML providers with JIT provisioning and claim → role mapping. The slice is done when SSO sign-in completes against a test provider, a SCIM round trip provisions a user, an approved request grants only inside its window, and the last-owner invariant blocks self-lockout.

### Risks / notes

- **Migration number** is “next free slot at build time”; the file stays additive. The `role_bindings` subject/scope change is expand-then-contract — keep writing `user_id` until a later release drops it.
- **One decision path:** the guard, the list filters and the simulator must call the same `crates/authorization` function; a second implementation is a release blocker because it drifts silently.
- **Precedence must stay SQL-expressible** for list filtering — benchmark resolution against a seeded 5k-binding organization and keep it inside a request budget.
- **Secrets by reference only:** provider credentials live behind `secret_ref` (environment or secret store, REQ-037), MFA secrets are stored encrypted, and codes are never logged.
- **Provisioning tokens** are prefix + hash and rotatable; the sync log drops personal data on retention, and deactivation (not deletion) is the SCIM default.
- **SSO, passkeys and event volume:** document the local `localhost` exception so QA can exercise WebAuthn (a test that silently skips is not evidence), and sample or aggregate `iam.policy_denied`/`iam.signin_failed` before they reach webhooks.

## Progress

### Slice 3a — Sessions, devices, the security policy and TOTP (shipped)

- **Migration `0017_iam_sessions_mfa.sql`**: `sessions.step_up_at`, a live-session index for the
  list, and `mfa_challenges` (a sign-in whose account holds a factor is finished by a code that
  consumes a short-lived challenge — the same table carries the step-up challenge). Additive;
  every table the slice writes already existed in `0011`.
- **`crates/identity` gained five modules.** `totp` implements RFC 6238/4226 over HMAC-SHA1 with
  the RFC's own test vectors, plus Base32 and the `otpauth://` URI; `secrets` encrypts stored
  secrets as encrypt-then-MAC envelopes (SHA-256 counter mode + HMAC-SHA256, key from
  `OMNION_MFA_KEY`); `security` is the policy document with the table's ranges as field-level
  refusals and a CIDR matcher for the address lists; `devices` fingerprints a user agent and
  keeps first/last seen and the trust window; `mfa` enrols TOTP in two steps and issues ten
  single-use recovery codes (hashed; consumed by a conditional update, so "exactly once" holds
  even under a race).
- **`signin` is the order of a sign-in**: the address lists and an existing lockout are checked
  **before** the password, then the password, then a confirmed factor — every attempt lands in
  `sign_in_attempts` with the word the reader sees (`failed`, `locked`, `blocked`, `mfa_required`,
  `success`). Lockout has two dimensions: a per-account counter that sets `users.locked_until`
  and a per-address count of recent failures, because an account lock alone lets one address
  spray every account forever.
- **Sessions take their lifetimes from the policy row, never from a constant**: the idle window
  is enforced when a token is resolved (and the row reads back as `idle`), the absolute lifetime
  is `sessions.absolute_expires_at`, and opening a session past the concurrent cap retires the
  oldest one (`revoke_reason = 'concurrent_cap'`). Revoking by id and signing an account out
  everywhere answer the ids they ended, and every one of them emits `iam.session_revoked`.
- **Dangerous operations demand a fresh step-up** (`auth/step-up`: the caller's own password or
  an enrolled code, ten minutes): resetting an account's factors, removing a confirmed factor and
  issuing a service-account key answer `403 step_up_required` with the action named. Forgetting a
  device also ends its live sessions.
- **Panel**: `/settings/iam/security` (five policy tabs with the ranges on the field, a save that
  answers the diff it applied), `/settings/iam/sessions` (state badges from the same values the
  resolver reads, filters, revoke, sign-out-all, cards below `lg`), `/settings/iam/devices`
  (trust window, forget) and the user detail's **Second factors** tab (enrolment with the secret
  shown once, confirmation, recovery codes, remove, reset — all through the step-up dialog).
- **Proof**: `cargo test --workspace` green — the slice walk
  (`apps/api/tests/iam.rs::sessions_devices_mfa_and_the_security_policy_are_proven_end_to_end`)
  drives the whole slice over HTTP: a policy save with its diff, a range refusal and an unusable
  network each naming their field, a lockout that triggers at exactly the configured threshold
  and is recorded, a denied address refused before the password check (asserted with the *correct*
  password), an allowlist narrowing, the per-address failure count, the idle window proven twice
  (the same row refused at five minutes and accepted at two hours), a revoke that ends a session
  on its next request, `sign-out-all`, the concurrent cap retiring the oldest, the device registry
  with its trust window, TOTP enrolment + confirmation + a challenge that a code completes, a
  recovery code that works exactly once, and step-up demanded for MFA reset and key issuance
  (with the audit trail carrying every one of those actions). The walkthrough drives the same
  screens in the browser (`scripts/qa/walkthrough.cjs`, pass `iam-security-depth`).
- **Remaining from this slice**: the `mfa_required` policy flag is stored and surfaced but does
  not yet force enrolment at sign-in. WebAuthn/passkeys shipped as slice 3b, below.

### Slice 3b — WebAuthn passkeys (shipped)

- **The ceremony lives in `crates/identity/src/webauthn/`**, in three pieces: `cbor.rs` is a small
  CBOR reader (definite lengths only — an indefinite or floating-point item is refused rather
  than guessed at), `cose.rs` reads a COSE credential public key and verifies **ES256** (P-256
  ECDSA over the DER signature WebAuthn carries) and **EdDSA** (Ed25519, strict verification, so
  the small-order keys that let a signature verify under two keys are rejected), and `mod.rs`
  runs the two ceremonies.
- **Registration** checks that the client data says `webauthn.create` and names the challenge
  this server issued, that the origin is one this installation serves, that the authenticator
  data hashes to the relying party id and reports a present user, that the attested credential
  extracts to a supported COSE key, and — when a `packed` statement carries a self signature —
  that the signature verifies over `authenticatorData || SHA-256(clientDataJSON)`. Attestation is
  requested as `none`, so the provenance of an authenticator is never claimed, only its key.
- **Assertion** (the sign-in) verifies the same challenge/origin/relying-party rules and the
  signature over the same message, and refuses a signature counter that does not move forward —
  the cloned-authenticator check. A counter-less authenticator (`0` and stored `0`) is accepted,
  which is the honest reading of a device that does not implement one.
- **The documented loopback exception.** Browsers treat `http://localhost` and `http://127.0.0.1`
  as secure contexts, and `OriginPolicy` accepts a loopback origin from any port unless
  `OMNION_WEBAUTHN_ALLOW_LOOPBACK=false`; `OMNION_WEBAUTHN_RP_ID` (default `localhost`) is the
  host a credential is bound to and `OMNION_WEBAUTHN_ORIGINS` pins the deployment's own origin.
  The exception is a first-class rule with its own test and the QA pass exercises a real ceremony
  over it, instead of a test that silently skips.
- **Migration `0018_webauthn.sql`**: `webauthn_challenges` (single-use, purpose-scoped,
  short-lived; one live challenge per account and purpose) plus the per-kind factor index. The
  factor rows themselves needed nothing: `0011` already carried the credential columns, the
  confirm-shape constraint and the unique live-credential index.
- **Routes**: `/api/v1/auth/webauthn/register/begin|complete`, `/passkeys` (list),
  `/passkeys/{id}` (remove, step-up) — enrolment runs behind the caller's own session, because a
  passkey belongs to the account at the keyboard and an administrator may only remove one
  (`/iam/users/{id}/reset-mfa`). The sign-in half (`authenticate/begin|complete`) sits beside
  `auth/mfa/verify`: the password check answers a challenge token, and a verified assertion
  consumes it and opens the same session a password sign-in opens (its auth methods record
  `password` and `webauthn`). Every ceremony refusal names the one check that did not hold
  (`400 webauthn_refused`), a spent or unknown challenge is its own error, and a credential that
  is already enrolled answers `409 credential_registered`.
- **Panel**: the user detail's **Second factors** tab gained a passkeys section — the
  self-service list with enrolment ("Add a passkey" runs `navigator.credentials.create` through
  `apps/admin/lib/webauthn.ts`), removal behind the step-up prompt, an empty state, and an honest
  sentence for an account other than the signed-in one. The sign-in screen now has the second
  step a factor demands: a code field, a recovery code, and **Use a passkey**
  (`navigator.credentials.get`), with the ceremony's own errors surfaced.
- **Proof.** `cargo test --workspace` green: the ceremony units (ES256 and EdDSA round trips, a
  packed self attestation verified and a broken one refused, wrong challenge/origin/relying
  party/cross-origin/counter refusals, the loopback policy) and
  `apps/api/tests/webauthn.rs::a_passkey_enrols_and_signs_in_end_to_end` over the real router.
  The QA walkthrough's `iam-passkeys` pass enrols a passkey on the owner's account with a Chrome
  **virtual authenticator**, reads the row back, signs in with the passkey after the password
  step, and removes it again (an account left with a passkey would break every later
  password-only sign-in the walk performs).
- **Remaining in this slice**: nothing. `mfa_required` still needs enforcing at sign-in.

### Slice 4a — The ABAC policy engine, the builder and the safety invariants (shipped)

- **`crates/policy-engine` is the pure half.** A condition tree (a node is `all`, `any`, `not` or a
  leaf of attribute → operator → value), the seven operators (`==`, `!=`, `>`, `<`, `in`,
  `starts_with`, `contains`), target patterns with `*` wildcards (`content.pages.*`), and the
  decision rule: the highest priority decides, equal priorities resolve to **deny**, and a
  disabled policy decides nothing. Two readings from docs/07 §11 are pinned by tests — **a missing
  attribute compares as null** (a dotted path that names nothing resolves to `null`, never to a
  fabricated value) and **deny wins**. Fourteen unit tests cover every operator, the grouping, the
  round trip of the stored shape, the malformed-condition refusals, the priority tie and the
  leaf-by-leaf trace.
- **`crates/permissions` gained `policies` and `invariants`.** `policies::apply` is the overlay the
  guard and the simulator now share: role bindings resolve first, then the organization's enabled
  policies get the last word — an allow policy grants what RBAC did not, a deny policy refuses what
  RBAC granted, and with no winner the RBAC answer stands. `Decision` grew
  `DenyReason::PolicyDeny`, `Via::Policy` and a `PolicyStamp`, so a 403 and the simulator report
  the deciding policy by name (the 403 `details.source.policy`, the simulator's `source.policy`
  and its per-policy verdict list). Attributes merge the account's stored `users.attributes` (at
  the top level and again under `user`) with the request facts (`action`, `subject.{type,id}`,
  `organization.id`, `resource.{site_id,path,department,module}`). Every save appends a
  `policy_versions` row, so history is complete from version 1; `PolicyDraft::validate` refuses a
  blank name, a priority outside 0–1000, an empty or duplicated target set and an unknown target
  key.
- **`invariants::check_binding_revocation`** runs in `DELETE /iam/bindings/{id}` before the row is
  touched. Two rules: the caller keeps a privileged binding of their own (`409 self_lockout`, the
  sentence names “your own last …”), and the scope class keeps one — an organization-scoped
  binding is defended by its organization, a global (platform) binding by the other global ones
  (`409 last_owner_binding`, “at least one …”). Both are specific to the built-in `owner` and
  `administrator` roles, both leave the store untouched when they refuse, and an expired binding is
  never defended (cleanup stays possible).
- **Routes**: `GET/POST /api/v1/iam/policies`, `GET/PUT/DELETE /api/v1/iam/policies/{id}`,
  `GET /iam/policies/{id}/versions` and `POST /iam/policies/{id}/test`. Reading needs
  `iam.policies.read`, saving `iam.policies.manage`, and the dry run only the read key because it
  writes nothing. The dry run accepts an **unsaved draft** alongside the stored policy and answers
  with the leaf-by-leaf trace (each leaf's expected and resolved value with its verdict) plus the
  verdict the organization's enabled policies would reach with this policy in place — including
  whether this policy would be the one that decides. Create, update and delete emit
  `iam.policy_changed` and land in the audit trail as `iam.policy.created|updated|deleted`.
- **Panel**: `/settings/iam/policies` — the list (effect badge, priority, target count, version,
  disabled state) beside the builder: **WHEN** (condition rows with ALL/ANY, a per-row NOT, and a
  JSON view for trees the rows cannot express), **THEN** (effect, target permissions with the
  catalogue as a datalist, priority, enabled) and **Test** (sample attributes, the highlighted
  leaves, the verdict and which policy would win). A save shows the new version; **History** lists
  every recorded version. The screen is stable on first paint (skeletons), refuses a malformed
  draft in the field itself before any request, and keeps its empty state actionable.
- **Proof (Rust).** `cargo test --workspace` → **653 tests, 0 failures** (exit 0; +23 on this
  slice: 16 policy-engine units, 6 overlay/attribute/validation units, 2 invariant units, and
  `apps/api/tests/iam_policy.rs::abac_policies_and_the_safety_invariants_are_proven_end_to_end`,
  which drives the whole slice over the real router: the member without `iam.policies.read` is
  refused, the deny policy removes `users.read` from the Owner (403 with `reason=policy_denied`
  and the policy named in `details.source.policy`), the allow policy grants the same permission to
  a member whose RBAC set provably does not carry it, raising the deny to priority 900 takes it
  back, disabling the deny returns the allow, the history reads three versions back, the dry run
  with a draft reports `applies=true` and a satisfied leaf with its resolved value (and
  `applies=false` when the path does not match), the four `invalid_policy` refusals answer 400,
  the invariants answer `self_lockout` then `last_owner_binding` then allow an ordinary
  revocation, and the audit trail plus the `iam.policy_changed` events are read back).
- **Proof (web).** `pnpm typecheck && pnpm build` → 2/2 (`@omnion/admin`, `@omnion/web`).
- **Proof (QA).** `bash scripts/qa/run.sh` → `qa-artifacts/20260927-112459`: **772 clicks, 803
  screenshots, 5 findings (0 high** — the five carried-forward public-renderer 404s**)**, vision
  review 0 issues / 0 failures, `Refusals provoked on purpose — 2` (the passkey step-up). The
  browser pass caught two real defects of this slice, both fixed in the tick: the condition rows
  took their ids from a module-level counter, so the server and the client rendered different
  markup (a hydration mismatch and 20 console errors), and the JSON view's parse escaped out of a
  render when the generic clicker typed junk into it. The first fix made the first render
  deterministic and labelled the five controls the same pass had flagged; the second made both
  JSON paths refuse in the field with a sentence instead of throwing. The walkthrough's new
  `iam-policies` pass fills the builder from its rows (name, effect, priority, target chip and one
  condition row), runs the dry run (a matched leaf is highlighted), saves and reads the version
  history back, removes the policy again (the two-press delete) and proves the in-field refusal
  for an out-of-range priority without a single failed request. The route is walked on desktop and
  mobile, so no screen of this slice is untested.
- **Remaining in this slice**: nothing. Slice 4b carries enterprise sign-in (OIDC/OAuth2/SAML),
  SCIM 2.0 provisioning and permission requests/approvals. It is split in two: **4b-1** (shipped)
  is permission requests + approvals and SCIM 2.0 provisioning; **4b-2** (pending) is enterprise
  sign-in.

### Slice 4b-1 — Permission requests, approvals and SCIM provisioning (shipped)

- **Where**: `crates/permissions/src/approvals.rs` (requests, the decision, the generated grant
  role), `crates/identity/src/provisioning.rs` (tokens + sync log), `apps/api/src/routes/`
  `iam_approvals.rs` and `iam_provisioning.rs` (the panel's API) and `scim.rs` (the SCIM 2.0
  surface). No migration: the tables shipped with `0011_iam_advanced.sql`
  (`permission_requests`, `provisioning_tokens`, `provisioning_log`, `auth_providers`), and this
  slice is the first thing to read or write them.
- **An approval is a binding, not a flag.** A request carries a permission key and an optional
  resource pattern; an approval requires a window (5 … 43200 minutes) and produces a real
  time-boxed binding: a generated role per (organization, permission key) — `grant-users-read`
  for `users.read`, priority 100, exactly one `allow` entry — bound to the requester until
  `expires_at`. The resolver stops counting it on its own, the list route retires the lapsed row
  as `expired`, and nothing has to sweep or remember anything. The two writes (the binding, the
  decision) commit in one transaction, so an approval can never exist without its grant.
- **Asking is open, deciding is not.** `POST /iam/requests` needs only a signed-in session —
  the person who cannot do something is exactly the person who must be able to ask — while the
  inbox needs `iam.approvals.read` and a decision needs `iam.approvals.decide`. A decided request
  answers `409 request_already_decided`; the window is refused in the field
  (`invalid_request: the window must be between 5 and 43200 minutes`); an unknown key never
  becomes a pending row.
- **SCIM 2.0** (`/api/v1/scim/v2`): `Users` and `Groups` with `ListResponse` envelopes, a small
  explicit filter surface (`userName` / `externalId` / `displayName` with `eq`, refused with
  `invalidFilter` otherwise), `PatchOp` with `Operations` (`replace`, `add`, `remove`), and
  `DELETE` meaning **deactivate** — the SCIM default, because a directory sync never means
  "delete this person's history". `ServiceProviderConfig` and `Schemas` answer without a token; a
  session cookie is not a token and is refused, so the surface cannot be driven from a browser
  tab that happens to be signed in.
- **Tokens are minted once and hashed**: `omsc_<prefix>_<secret>`, the secret returned exactly
  once, the stored value a SHA-256 hex digest compared in constant time; `last_used_at` moves on
  every call, and revocation takes effect on the next request. Every write lands in
  `provisioning_log` with its action and outcome (ids and outcomes, never payloads).
- **Panel**: `/settings/iam/approvals` (tabs with counts, the ask form, approve with a window or
  refuse with a note, the requester's own requests) and `/settings/iam/provisioning` (tokens with
  the secret shown once and revoke-armed-again, the sync log, and the note that DELETE
  deactivates). Both name the organization for a platform account, the same pattern the policies
  screen already uses.
- **Proof (Rust).** `cargo test --workspace` → green, including
  `apps/api/tests/iam_approvals.rs::an_approved_request_grants_only_inside_its_window` (a member
  is refused, asks, cannot decide, is granted a 30-minute window that really grants, loses it the
  moment the window passes without anybody acting, reads `expired`, is refused a second time, and
  the audit trail carries requested/approved/rejected) and
  `apps/api/tests/scim.rs::a_scim_round_trip_provisions_and_logs` (mint → create → filter →
  patch → deactivate over the real router with a real token, the secret never in the database,
  the log carrying each outcome, and a revoked token refused on its next call).
- **Proof (web).** `pnpm typecheck && pnpm build` green with both routes in the table.
- **Proof (QA).** `bash scripts/qa/run.sh` → `qa-artifacts/20260927-175102`: 29 pages, 855 clicks,
  887 screenshots, **0 high findings** (5 medium: the carried-forward public-renderer root 404s),
  vision 3 items on REQ-007's analytics screens. The `iam-approvals` pass reads a create → approve
  (window chip) → reject → in-field refusal, and `iam-provisioning` a mint → SCIM create/patch →
  sync log → revoked `401`, on desktop and mobile.
- **Remaining in this slice**: nothing. Slice 4b-2 carries enterprise sign-in (OIDC/OAuth2/SAML
  providers with JIT provisioning and claim → role mapping).


### Slice 2 — Subjects, scopes and the simulator (shipped)

- **Migration `0016_iam_subjects.sql`** completes the expand-then-contract move: `user_id` becomes
  optional, the backfill trigger only fires while a writer still speaks `user_id`, and liveness is
  re-keyed on the subject (plus the resource a binding names) — so `/blog/*` and `/legal/*` can
  both carry a binding. Additive: applied to the populated development database and to the QA
  database, and the unit suite re-applies migrations from scratch.
- **The subject model** (`crates/permissions/src/model.rs`): `Subject::{User, Group,
  ServiceAccount}` with `describe()`; the scope ladder `Global → Organization → Site →
  Department → Module → Resource`; `ResourceContext` carries the organization, site, department,
  module and path a question is asked in; `matching.rs` turns a binding's resource glob
  (`/blog/*`, `/blog/**`, exact paths) into a matcher and reports what it matched.
- **Groups and service accounts** (`groups.rs`, `service_accounts.rs`): membership is a row, not a
  second role table; a group binding applies to every member, and a machine identity holds keys
  (prefix + hash, secret returned exactly once, revocable per key).
- **The simulator** (`simulate.rs`): one function answers `allowed` / `explicit_deny` /
  `missing_permission` with the deciding role, the `via` and a step list where every binding is
  reported as `active`, `out_of_scope`, `expired` or `revoked` — the same resolution the guard
  runs, so the two cannot drift.
- **API**: `/api/v1/iam/{users,groups,service-accounts,simulations,effective-permissions}`,
  bindings that accept a subject and any scope from the ladder with `expires_at`, and the
  simulator route guarded by `require_or_machine` so a service-account key authenticates over
  `Bearer` while an interactive sign-in stays impossible for a key.
- **Refusals explain themselves**: every `403 permission_denied` carries `details` — the
  permission, the `reason` (`missing_permission` / `explicit_deny`), the `source` role when one
  refused it, the context and how many bindings were consulted (docs/07-IAM.md §18). The verdict
  in the body is the verdict the simulator shows.
- **Seeding is a reconciliation, not a one-off insert**: every base role now tracks the catalogue
  on boot — a key added after an installation was seeded reaches the roles that declare it, a row
  that lost its effect is repaired, and a key the code dropped is pruned. Before this, an
  existing deployment's Administrator silently missed every key added later (measured: 32 of 72).
- **Panel**: `/settings/iam` (overview), `/settings/iam/users` + `/settings/iam/users/{id}`,
  `/settings/iam/groups`, `/settings/iam/service-accounts` (key shown once, revoke per key) and
  `/settings/iam/simulator` (subject picker, resource path, verdict card with the step list).
- **Proof**: `cargo test --workspace` green — the slice-2 walk
  (`apps/api/tests/iam.rs::subjects_scopes_and_the_simulator_are_proven_end_to_end`) proves group
  membership granting and revoking over HTTP, resource-scoped bindings with an expiry that stops
  counting without deleting the row, a machine key authenticating a `/api/v1` request and failing
  to start a session, the 403 that names its decision source, the seeding reconciliation, and a
  simulator-vs-guard matrix of ≥ 200 (subject, action, resource) cases. The walkthrough drives the
  same screens in the browser (`scripts/qa/walkthrough.cjs`, pass `iam-subjects`).
- **Next**: slice 3 — sessions, devices, MFA and the security policy (idle/absolute lifetime,
  lockout, IP lists, TOTP + passkey, step-up).

### Slice 1 — Role depth (shipped)

- **Migration `0011_iam_advanced.sql`** carries the whole request's data model (role versions,
  devices, MFA factors and recovery codes, groups, service accounts and their keys, ABAC
  policies and versions, security policy, sign-in attempts, providers, permission requests,
  provisioning tokens and log), the `users`/`sessions` extensions and the expand-then-contract
  move of `role_bindings` to subjects and the wider scope ladder. Verified twice: applied to a
  scratch database (fresh) and to the populated development database (backfill + seed read
  back).
- **Role depth** (`crates/permissions`): `update_role`, `delete_role` (refusing a role with live
  bindings), `duplicate_role`, `ancestors`/`children`, one validation function that refuses a
  cycle and a chain past eight levels in either direction, and `replace_role_permissions` — the
  atomic matrix save with `expected_version`, a diff and a recorded version. `crate::versions`
  appends and reads `role_versions` and computes diffs (pure, unit-tested).
- **API** (`/api/v1/iam/roles/*`): detail with entries, chain, children and member count;
  `PATCH`, `DELETE`, `POST /duplicate`, `POST /preview`, `GET /versions` and `GET /members`;
  the matrix save answers with the diff it applied. Errors carry field-level codes
  (`role_inheritance_cycle`, `role_inheritance_depth`, `role_has_bindings`,
  `role_version_conflict`, `invalid_entries`). The catalogue gained the IAM family the later
  slices guard their routes with.
- **Panel**: `/settings/iam/roles` (platform + custom roles, counts, create, open, duplicate,
  two-step delete) and `/settings/iam/roles/{id}` (the matrix with tri-state cells, category
  accordion, per-category counts and grant/deny/inherit-all, a filter, a diff preview, a sticky
  Save/Discard footer, plus Members, Inherited by and History tabs where the history draws each
  version's diff).
- **Tenant scope on the list**: a role belongs to a tenant, so `GET /iam/roles` answers platform
  roles plus one organization's — a tenant account always for its own, a platform account for
  the tenant it names with `?organization_id=` (and the panel gives it a tenant picker). Without
  that, a platform owner could create a role for a tenant and then not see it — the QA pass
  caught exactly this.
- **Refusals the reader sees, not round-trips**: the role editor refuses an empty name and a
  priority outside `0–1000` in the field itself (the API refuses the same shapes, pinned by the
  integration walk), so a mistyped form never becomes a `400` in the console.
- **Proof**: `cargo test --workspace` green — the role-depth walk
  (`apps/api/tests/iam.rs::role_depth_lifecycle_is_proven_end_to_end`) proves the lifecycle over
  HTTP: create → set → duplicate → delete-while-unbound, the cycle/self/depth refusals, the
  atomic refusal leaving the set untouched, the stale-version refusal, the history diffs and
  precedence through the guard. The walkthrough drives the same path in the browser
  (`scripts/qa/walkthrough.cjs`, pass `iam-roles-depth`).
- **Next**: slice 2 — users screen, bindings at every scope level with expiry, groups, service
  accounts and their keys, effective permissions and the RBAC simulator.
