# REQ-066 — MFA, Passkeys & Device Trust

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Account protection beyond passwords.

- TOTP MFA (enrol, verify, recovery codes), optional WebAuthn/passkey login.
- Login policies (require MFA for roles, allowed login hours, IP allow/deny lists).
- Password policies (length, complexity, rotation hint, breached-password check hook).
- Session management screen: active sessions with device, IP, last seen, revoke.
- Device management: trusted devices, revoke trust, "remember this device" window.
- Step-up authentication for dangerous operations (deploy, delete organization, key rotation).

## Implementation spec

### Scope (in / out)

**In**
- TOTP: enrolment (QR payload + manual secret shown once), code verification with a ±1 step window, replay protection (the last accepted step is stored), factor removal, and single-use hashed recovery codes with a remaining-count display.
- WebAuthn / passkeys: registration and authentication ceremonies (platform and roaming authenticators, user verification required, resident keys welcome), passkey list per user with label, added, last used and sign count; a passkey is a valid first factor and a valid second factor; a sign-count regression flags the device for review.
- Login policy, one document per organization: MFA required for named roles (with a grace period for onboarding), allowed sign-in hours and weekdays in the organization timezone, IPv4/IPv6 CIDR allow and deny lists (deny wins, evaluated before the password check), lockout thresholds, and session lifetime settings.
- Password policy: minimum length, required character classes, rotation hint interval, password history depth, and a breached-password check hook (anonymized prefix + count response) whose failure mode is configurable (`warn` or `block`).
- Session management: self-service list (current session flagged) with revoke-one and sign-out-everywhere; admin list with per-user revoke and `sign out everywhere`; idle, absolute and concurrent-session limits enforced from the policy row, never constants.
- Device trust: a known-device registry (fingerprint hash, label, platform, browser, first/last seen), trust windows, "remember this device", revoke trust, and a `device_first_seen` notice that the next sign-in from a new device requires the second factor.
- Step-up authentication: a short-lived, operation-scoped grant required by dangerous operations (deployment deploy/rollback, organization delete, key and token issuance, MFA reset, security-policy changes); the dialog records the attempt and the guarded action refuses without a fresh grant.
- Admin MFA reset (`users.update` plus step-up) that clears factors and forces re-enrolment at the next sign-in; every security change writes an audit entry with a before/after diff.

**Out**
- Provider-based first factors (REQ-065); role and policy authoring (REQ-067, REQ-069); approval-based access windows (REQ-073); notification rendering for security alerts (REQ-021); anomaly scoring (REQ-012).
- Hardware-token fleet management and certificate-based smart-card sign-in beyond what WebAuthn covers.
- Password managers, browser extension support, or anything that stores credentials on the platform's behalf.

### Screens (UI)

Nav: **Account → Security** (self-service) and **Settings → IAM → Security · Sessions · Devices**.

| Route | Screen |
|---|---|
| `/settings/account/security` | Password, MFA, passkeys, recovery codes, my devices, my sessions |
| `/settings/iam/security` | Policy tabs: Sign-in · Password · Sessions · Devices · Step-up |
| `/settings/iam/sessions` | All active sessions |
| `/settings/iam/devices` | Known devices with trust state |
| (shared) Step-up dialog | Rendered by dangerous operations; never a full page |

- **Self-service security.** Cards in this order: Password (change form with a strength meter and the policy hints inline), Two-factor (status chip `on` / `off` / `required for your role`, `Enrol` wizard — QR block plus a monospace manual secret with copy, a `Enter the 6-digit code` step, then recovery codes as a monospace list with copy/download and an `I have saved these` confirm), Passkeys (list: Label, Added, Last used, Actions `Remove`; `Add a passkey` runs the browser ceremony and asks for a label), Trusted devices (Label, Platform, First seen, Trusted until, `Revoke trust`), Sessions (`This device` badge on the current row, Revoke, `Sign out everywhere`). Each destructive action re-checks (password or step-up) per policy.
- **Sign-in policy tab.** MFA requirement: role multi-select with an always-on hint when a bound role still needs MFA; grace period in days `0–30`. Allowed hours: weekday checkboxes plus `from`/`to` time pickers evaluated in the organization timezone, with an explicit `no restriction` default. IP lists: two textareas (allow, deny) with per-line CIDR validation, a count of covered addresses, and a warning when deny would block the editor's own current IP. Lockout: attempts `3–50`, window and duration `1–1440` minutes. Saving shows a diff of what changed and writes one audit entry.
- **Password tab.** Minimum length `8–128`, class switches (upper, lower, digit, symbol) with at least two required, rotation hint `0–730` days (`0` = never) shown as an advisor only, history `0–24`, breach-check hook: reference field, `warn`/`block` radio, and a `Test the hook` button. Out-of-range values produce field-level errors, never a silent clamp.
- **Sessions and devices (admin).** Sessions table: User, Device, IP, Methods (chips), Started, Last seen, Expires, Actions; filters by user, method, IP prefix and sign-in range; bulk `Revoke`. Devices table: User, Label, Platform, Browser, First seen, Last seen, Trusted until, Status (`trusted`, `untrusted`, `revoked`); bulk `Forget`, `Trust`, `Revoke trust`; row drawer shows the last five sign-ins from that device.
- **Step-up dialog.** Opened by a dangerous action: title names the operation, method picker (passkey first when registered, then authenticator code, then a recovery code), a 5-minute progress hint, failure reasons inline (`code rejected`, `passkey cancelled`), and on success the original action resumes automatically. A stale grant shows `Your security check expired — confirm again`.
- **States, keys, mobile.** Skeletons, real empty states (`No passkeys yet` with the primary action), error strips with retry; secrets and codes are never re-rendered after their one-time display. Keys: `/` search, `j`/`k` rows, `r` revoke (confirm), `t` trust, `Esc` closes. Below `lg`, tables become cards, cards stack in one column, and the step-up dialog becomes a bottom sheet with the same method order.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · PUT | `/api/v1/iam/security-policies` | Read · write the organization security policy document | `iam.security.read` · `iam.security.manage` |
| GET | `/api/v1/me/security` | Own overview: factors, devices, session count, last sign-in | signed-in session |
| POST | `/api/v1/me/password` | Change own password (current password + policy + breach hook) | signed-in session |
| POST | `/api/v1/me/mfa/totp/start` · `/confirm` | Start enrolment (secret shown once) · confirm with a code | signed-in session |
| POST | `/api/v1/me/mfa/recovery-codes` | Regenerate codes (invalidates the previous batch) | signed-in session + step-up |
| GET · DELETE | `/api/v1/me/mfa/factors` (+`/{id}`) | List own factors · remove one | signed-in session (+ step-up) |
| POST | `/api/v1/me/passkeys/options` · `/verify` | WebAuthn registration ceremony | signed-in session |
| GET · DELETE | `/api/v1/me/passkeys` (+`/{id}`) | List · remove own passkeys | signed-in session |
| POST | `/api/v1/auth/mfa/verify` | Second-factor challenge after the password step | public sign-in route |
| POST | `/api/v1/auth/step-up` | Grant for a dangerous operation (operation key required) | signed-in session |
| GET · DELETE | `/api/v1/me/sessions` (+`/{id}`) | Own active sessions · revoke one | signed-in session |
| GET · DELETE · POST | `/api/v1/iam/sessions` (+`/{id}`, `/users/{id}/sign-out-all`) | Admin session management | `iam.sessions.read` · `iam.sessions.revoke` |
| GET · POST · DELETE | `/api/v1/iam/devices` (+`/{id}/trust`, `/{id}/revoke`) | Known devices · trust · revoke trust · forget | `iam.devices.read` · `iam.devices.manage` |
| POST | `/api/v1/iam/users/{id}/reset-mfa` | Clear factors after step-up; user re-enrols at next sign-in | `users.update` + step-up |

Dangerous routes call one shared helper that requires a fresh step-up grant for the operation key; the grant is bound to the session, the operation and the policy's freshness window, and is single-use for key and token issuance. Sign-in routes answer 429 with a retry hint when throttled by lockout.

### Data model

Migrations: `0118_mfa_factors_devices.sql`, `0119_login_security_policy.sql` (reserved band 0116–0125 for the identity & access wave; append-only ledger — take the next free number if taken). Base tables are shared with the IAM core migration (REQ-006): one table per concept — create what is missing, otherwise stay additive.

- `mfa_factors` (id uuid pk, user_id uuid → users on delete cascade, kind text in ('totp','webauthn'), label text, secret_ciphertext text null — encrypted at rest, credential_id text null, public_key text null, sign_count bigint default 0, transports text[] default '{}', aaguid text null, confirmed_at timestamptz null, last_used_at timestamptz, disabled_at timestamptz null, created_at) — unique (user_id, credential_id) where credential_id is not null; index (user_id) where disabled_at is null.
- `mfa_recovery_codes` (id uuid pk, user_id uuid cascade, code_hash text, batch int default 1, used_at timestamptz null, created_at) — index (user_id) where used_at is null.
- `user_devices` (id uuid pk, user_id uuid cascade, fingerprint_hash text, label text default '', platform text, browser text, first_seen_at timestamptz, last_seen_at timestamptz, trusted bool default false, trust_expires_at timestamptz null, revoked_at timestamptz null, revoked_by uuid null) — unique (user_id, fingerprint_hash); index (user_id) where revoked_at is null.
- `sessions` += `device_id` uuid null → user_devices on delete set null, `auth_methods` text[] not null default '{}', `absolute_expires_at` timestamptz null, `revoked_by` uuid null, `revoke_reason` text null — index (user_id) where revoked_at is null, index (device_id).
- `security_policies` (organization_id uuid pk → organizations on delete cascade, mfa_required bool default false, mfa_role_ids uuid[] default '{}', mfa_grace_days int default 0, allowed_days smallint[] default '{}', allowed_from time null, allowed_to time null, ip_allowlist cidr[] default '{}', ip_denylist cidr[] default '{}', lockout_attempts int default 10, lockout_minutes int default 15, password_min_length int default 12, password_require_classes smallint default 2, password_rotation_hint_days int default 0, password_history int default 5, breach_check_url text null, breach_check_mode text in ('warn','block') default 'warn', session_idle_minutes int default 480, session_absolute_days int default 30, session_concurrent_cap int default 10, device_trust_days int default 30, step_up_minutes int default 10, step_up_operations text[] default '{deployment.deploy,deployment.rollback,organizations.delete,iam.provisioning.manage,iam.serviceaccounts.key_issue}', updated_by uuid, updated_at) — checks keep every range; a row is created lazily on first read with conservative defaults.
- `password_history` (id uuid pk, user_id uuid cascade, password_hash text, created_at) — index (user_id, created_at desc).
- `step_up_grants` (id uuid pk, user_id uuid cascade, session_id uuid → sessions on delete cascade, operation text, method text in ('passkey','totp','recovery'), granted_at timestamptz, expires_at timestamptz, consumed_at timestamptz null) — index (session_id, expires_at).
- `sign_in_attempts` (id uuid pk, email text null, user_id uuid null, ip inet null, user_agent text null, method text, outcome text in ('ok','bad_password','mfa_required','mfa_failed','locked','ip_denied','hours_denied','breach_blocked'), created_at) — index (lower(email), created_at desc), index (ip, created_at desc).
- `users` += `mfa_enforced` bool default false, `failed_sign_in_count` int default 0, `locked_until` timestamptz null, `password_changed_at` timestamptz null — index (locked_until) where locked_until is not null.

TOTP secrets are encrypted with a platform key and never readable back; recovery codes and device fingerprints are hashes; nothing in this model returns a code, secret or raw fingerprint through the API.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `iam.mfa_enrolled` · `iam.mfa_reset` | Enrolment and admin reset | `user_id`, `kind`, `actor_user_id` |
| `iam.mfa_challenge_failed` | A wrong or replayed code/passkey assertion | `user_id`, `method`, `reason` code |
| `iam.recovery_codes_regenerated` · `iam.recovery_code_used` | Recovery code lifecycle | `user_id`, `batch` |
| `iam.passkey_registered` · `iam.passkey_removed` | Passkey lifecycle | `user_id`, `factor_id`, `aaguid` |
| `iam.password_changed` | Password change with policy result | `user_id`, `source` (`self`,`admin`,`reset`) |
| `iam.session_revoked` · `iam.user_signed_out_everywhere` | Session management | `session_id`, `user_id`, `by` |
| `iam.device_first_seen` · `iam.device_trusted` · `iam.device_trust_revoked` | Device lifecycle | `user_id`, `device_id`, `platform` |
| `iam.step_up_granted` · `iam.step_up_failed` | Step-up attempts | `user_id`, `operation`, `method` |
| `iam.signin_denied` · `iam.account_locked` | Policy refusals with the rule named | `user_id`, `reason` code (`ip_denied`,`hours_denied`,`locked`) |
| `iam.security_policy_changed` | Policy save | `organization_id`, changed field names |

Consumed: `user.created` (apply the policy's grace rules), `iam.provider_updated` (SSO sign-ins re-check the MFA requirement). Webhook relevance: the security centre (REQ-012) subscribes to `iam.account_locked`, `iam.mfa_challenge_failed` and `iam.step_up_failed` — sample these before they reach webhooks. Payloads carry ids, methods and reason codes; never codes, secrets, raw fingerprints or IP addresses.

### Acceptance criteria

- [ ] TOTP enrols from a fresh account: the QR payload and manual secret match, a code from the current step confirms, and a code from a consumed step is refused (replay).
- [ ] Recovery codes are shown once, stored hashed, and each works exactly once; regenerating a batch invalidates the previous one; the remaining count is accurate.
- [ ] A passkey registers against a virtual authenticator, appears in the list with a label, signs in as the first factor, and a sign-count regression flags the device.
- [ ] With MFA required for a bound role, sign-in stops at the challenge; the correct factor completes it, and no session row exists before the challenge passes.
- [ ] A denied IP is refused before any password check; deny wins over allow; an address outside allowed hours is refused with the rule named and in the organization timezone.
- [ ] Lockout triggers at the configured attempt count, records outcomes in `sign_in_attempts`, and clears after the configured duration.
- [ ] Password policy fields are enforced on change and on reset; a value outside a range is refused with a field error; the breach hook in `block` mode refuses a known-breached password and in `warn` mode lets it through with a warning.
- [ ] Password history refuses reuse of the last N passwords; the rotation hint appears when the age passes the threshold and never blocks sign-in.
- [ ] The self-service session list marks the current device, revoking another session kills its next request, and `Sign out everywhere` clears all sessions with one event per session.
- [ ] The admin session list shows device, IP, methods and timestamps, and bulk revoke revokes exactly the selected rows.
- [ ] The device registry records first/last seen, a trusted device skips the second factor inside its trust window, revoking trust forces the challenge on the next sign-in, and trust windows respect the policy maximum.
- [ ] The concurrent-session cap is enforced from the policy row; exceeding it refuses the oldest excess or refuses the new sign-in per the documented rule.
- [ ] A dangerous operation without a fresh step-up is refused with the operation named; after a passkey step-up the action completes and the grant expires after the policy window.
- [ ] A step-up for a different operation does not satisfy the guarded route, and a stale grant produces the expiry message instead of a generic error.
- [ ] Admin MFA reset clears factors, forces re-enrolment at the next sign-in, and is itself refused without step-up.
- [ ] A security-policy save writes an audit entry with a before/after diff; every new screen renders at 390 px with no horizontal scroll and the walkthrough reports zero high findings.

### QA plan

The walkthrough must visit `/settings/account/security` and complete a TOTP enrolment by reading the shown secret and computing the current code, save the recovery codes, add a passkey through a virtual authenticator, then sign out and sign in with the passkey; visit `/settings/iam/security` (toggle MFA requirement for a role, submit an out-of-range password length and assert the field error, submit an invalid CIDR and assert the line error), `/settings/iam/sessions` (revoke one row, then revoke all for a user), `/settings/iam/devices` (trust a device, revoke its trust), and trigger step-up on a dangerous operation twice — once with a cancelled dialog (action refused) and once completed. Visual check: the QR block renders as a real code, recovery codes sit in a monospace block with copy/download, factor and method badges show real states, the step-up dialog names the operation and resumes it, no secret or code remains in the DOM after enrolment, and screenshots `page-account-security`, `page-account-mfa-enrol`, `page-iam-security-policy`, `page-iam-sessions`, `page-iam-devices`, `mobile-account-security` are produced.

### Slices

1. **Factors and self-service enrolment.** Migrations for factors and recovery codes, TOTP start/confirm with replay protection, passkey registration and authentication ceremonies, the self-service security screen, and the password change form with policy checks. *Done when:* acceptance 1–3 and 7 pass and enrolment plus passkey sign-in are exercised in the walkthrough.
2. **Login and password policy.** Policy document, the five policy tabs with range validation, IP allow/deny with deny-wins, allowed hours in the organization timezone, lockout, history, and the breach hook. *Done when:* acceptance 4–8 pass, including a refused sign-in for each policy reason.
3. **Sessions and devices.** Session deltas with methods and absolute lifetime, idle/absolute/concurrent enforcement from the policy row, admin and self lists with revocation, the device registry with trust windows, and the new-device notice. *Done when:* acceptance 9–12 pass and both list screens are in the walkthrough inventory.
4. **Step-up and admin reset.** The step-up grant model and helper, the shared dialog, the operation list in policy, admin MFA reset, and the audit/event wiring. *Done when:* acceptance 13–16 pass and a guarded operation is proven both refused and allowed in one QA run.

### Risks / notes

- **One-time displays.** The TOTP secret and recovery codes are visible exactly once; the confirm step must clear them from component state, and no log, event or audit entry may carry them.
- **Clock skew.** TOTP accepts ±1 step and stores the last accepted step; widening the window further trades replay protection for convenience and needs a policy decision, not a code default.
- **Passkey RP ID.** WebAuthn requires the panel origin to be a secure context; document the local development exception so the QA pass exercises a real ceremony — a silently skipped ceremony is not evidence.
- **Fingerprint privacy.** Device fingerprints are hashed with a platform salt; the registry shows labels and coarse platform hints, never a raw fingerprint or full user agent.
- **IP evaluation order.** Deny is checked before allow and before the password check; IPv4-mapped IPv6 addresses are normalised first, otherwise a deny rule silently misses.
- **Step-up UX.** A grant is bound to session, operation and window — too short and operators click through blindly, too long and it is theatre; keep the default at 10 minutes and show the remaining time.
- **Concurrent cap semantics** must be written down (which session wins) before implementation, or two developers will implement opposite rules.
- **Event volume.** `iam.mfa_challenge_failed` and `iam.step_up_failed` can be noisy on shared networks — sample or aggregate before they reach webhooks (REQ-012), and never include the attempted code.
