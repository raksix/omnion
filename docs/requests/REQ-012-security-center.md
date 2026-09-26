# REQ-012 — Security Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A security dashboard inside the admin:

```text
Security Center

✓ MFA enabled
✓ HTTPS
✓ Secure cookies
✓ Database encrypted
⚠ 2 plugins outdated
⚠ 1 critical vulnerability
✓ Backup healthy
```

Plus, as features:

- rate limiting
- brute-force protection
- CSRF
- CSP
- security headers
- IP allow/deny
- audit logs
- secret management
- vulnerability scanner

## Implementation spec

New crate `crates/security` (posture checks, rate-limit and lockout policy, IP access rules, header policy, finding store) plus the admin section `/security`. It reads from `crates/audit` (security events), `crates/permissions` (permission catalogue) and the backup subsystem's status; it never holds a secret value of its own.

### Scope (in / out)

**In**

- Posture checks with a recorded last result per check: MFA availability and adoption, HTTPS/TLS termination, secure cookie flags, database encryption at rest (reported by the deployment as a configuration fact), backup health (last successful backup age), rate limiting enabled, CSP present and enforced, IP allow-list configured, dependency freshness, open findings by severity, secret rotation age.
- Findings store fed by (a) the platform's own config checks, (b) an ingested dependency-report file produced by CI (JSON), and (c) an operator upload of the same format. No outbound scanning calls.
- Rate limiting: per-scope limits (global, sign-in, public API, authenticated API, webhook intake) enforced by `apps/api` middleware backed by the Redis client in `crates/core`.
- Brute-force protection: failed-sign-in counting per account and per client IP, progressive delays and account lockout policy with unlock affordances and a lockout event.
- Security headers applied to API and admin responses: HSTS, CSP (report-only or enforce), X-Content-Type-Options, Referrer-Policy, Permissions-Policy, frame-ancestors, plus CSRF protection for cookie-authenticated mutations (double-submit token).
- IP allow/deny lists with CIDR matching, expiry and an evaluator that runs before route guards.
- Secret inventory (read-only): names, scopes, last-rotated dates and which integration references each secret — values are never listed, rendered or exported. Management itself belongs to the secrets manager request; this screen only reports.
- Security event view over the audit trail (sign-ins, lockouts, permission denials, header/settings changes, IP-rule changes) with filters and CSV export.

**Out**

- A hosted scanning service, penetration testing, runtime intrusion detection, WAF/bot mitigation (that stays at the CDN layer), compliance report packs (a separate compliance request), key rotation execution (secrets manager), and any change to the sign-in factors themselves (IAM scope).

### Screens (UI)

- `/security` — overview. Posture score ring plus check rows: **Check · State (pass/warn/fail/ unknown) · Detail · Last checked · Action**. Pass rows read as confirmed facts; warn/fail rows carry a working link (to `/security/headers`, `/backups`, the findings tab, …). Header actions: "Run checks" (re-evaluates now), "Last scan: …". Empty state before the first run with a single primary action; a failing check that cannot be evaluated shows `unknown`, never a fake pass.
- `/security/findings` — table: **Severity · Title · Component · Version · Fixed in · Source · Status · First seen · Last seen · Due**. Filters: severity, status, source, component, date range, free-text. Row opens a detail drawer: description, evidence (the report entry), timeline, notes. Actions: acknowledge, ignore with a required reason and optional expiry, mark fixed, reopen, create a follow-up task note. Bulk: acknowledge, ignore, export CSV.
- `/security/sign-in-protection` — form: failed-attempt window seconds (60–86400), attempt threshold (1–50), lockout duration minutes (1–1440), progressive delay switch, exempt IP rules, plus a live table of currently locked accounts with an "Unlock" action. Validation messages for out-of-range numbers and an empty threshold.
- `/security/rate-limits` — table of scopes: **Scope · Window (s) · Limit · Burst · State · Updated by**. Inline edit per row, enable/disable toggles, and a built-in tester: method + path + client IP → "would be limited / allowed", so an operator can verify a policy without guessing.
- `/security/headers` — header policy form: CSP directives as editable rows (directive, value list) with a rendered preview of the exact header line, mode radio (report only / enforce), HSTS max-age + includeSubDomains + preload, X-Content-Type-Options, Referrer-Policy select, Permissions-Policy, frame-ancestors; "Preview on this panel" applies the draft headers to the next response and shows the result. Save validates that a directive name is not empty and that a mode is chosen.
- `/security/ip-access` — two tables (allow list, deny list): **CIDR · Note · Added by · Added · Expires**. Create form validates CIDR/address syntax (both v4 and v6), warns when a rule would lock the current client IP out, and offers an "test an address" field showing the verdict and which rule matched (deny always wins over allow).
- `/security/events` — security-event table from the audit trail: **When · Actor · Action · Client IP · User agent · Outcome**. Filters mirror the audit screen; export CSV; row opens the full audit entry.
- States and input: skeletons while loading, retry on error, unsaved-changes guard on the header and limiter forms. Keyboard: `/` filter, `a` acknowledge selected finding, `⌘K` palette, `Esc` closes drawers. Mobile: cards instead of tables, forms single-column, the CSP preview scrolls horizontally inside its own container rather than the page.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/security/overview` | Posture checks with last results | `security.read` |
| POST | `/api/v1/security/checks/run` | Re-evaluate all posture checks | `security.scan` |
| GET | `/api/v1/security/findings` | Finding list (paged, filtered) | `security.read` |
| GET | `/api/v1/security/findings/{id}` | Finding detail + evidence | `security.read` |
| PATCH | `/api/v1/security/findings/{id}` | Acknowledge / ignore / mark fixed / reopen | `security.manage` |
| POST | `/api/v1/security/findings/import` | Ingest a dependency/scan report file | `security.scan` |
| GET | `/api/v1/security/settings` | Limiter, lockout, header and policy settings | `security.read` |
| PUT | `/api/v1/security/settings` | Save policy settings | `security.manage` |
| POST | `/api/v1/security/rate-limits/test` | Dry-run a limiter decision | `security.read` |
| GET | `/api/v1/security/ip-rules` | Allow/deny list entries | `security.read` |
| POST | `/api/v1/security/ip-rules` | Add a CIDR rule | `security.ip.manage` |
| DELETE | `/api/v1/security/ip-rules/{id}` | Remove a rule | `security.ip.manage` |
| GET | `/api/v1/security/locked-accounts` | Accounts currently locked out | `security.read` |
| POST | `/api/v1/security/locked-accounts/{user_id}/unlock` | Unlock an account | `security.manage` |
| GET | `/api/v1/security/events` | Security events from the audit trail | `security.read` |

New catalogue keys (category `security`): `security.read`, `security.scan`, `security.manage`, `security.ip.manage`. Sign-in failures are additionally recorded as `user.login.failed` events.

### Data model

**`security_check_results`** — one row per check per run (latest run is what the panel reads). `id bigint generated always as identity pk`, `check_key text not null`, `state text not null` (`pass|warn|fail|unknown`), `detail jsonb not null default '{}'`, `run_id uuid not null`, `checked_at timestamptz not null default now()`. Index `(check_key, checked_at desc)`.

**`security_findings`** — `id uuid pk default gen_random_uuid()`, `source text not null` (`config|dependency|platform|report`), `severity text not null` (`critical|high|medium|low|info`), `title text not null` (1–200), `description text not null default ''`, `component text`, `component_version text`, `fixed_in text`, `status text not null default 'open'` (`open|acknowledged|fixed|ignored`), `ignore_reason text`, `ignored_until timestamptz`, `acknowledged_by uuid → users(id)`, `acknowledged_at timestamptz`, `first_seen_at timestamptz not null default now()`, `last_seen_at timestamptz not null default now()`, plus a computed `fingerprint text not null` (component + title hash). Constraint: `status = 'ignored'` requires a non-empty `ignore_reason`. Unique index `(fingerprint, component_version)`.

**`security_settings`** — single row: `id smallint pk default 1 check (id = 1)`, `rate_limits jsonb not null default '{}'`, `lockout jsonb not null default '{}'`, `headers jsonb not null default '{}'`, `csp_mode text not null default 'report_only'` (`report_only|enforce`), `updated_by uuid → users(id)`, `updated_at timestamptz not null default now()`.

**`security_ip_rules`** — `id uuid pk default gen_random_uuid()`, `kind text not null` (`allow|deny`), `cidr cidr not null`, `note text not null default ''`, `expires_at timestamptz`, `created_by uuid → users(id)`, `created_at timestamptz not null default now()`. Unique `(kind, cidr)`; index `(kind)`.

**`security_login_attempts`** — `id bigint generated always as identity pk`, `email_hash text not null`, `client_ip inet`, `succeeded boolean not null`, `created_at timestamptz not null default now()`. Indexes `(email_hash, created_at desc)`, `(client_ip, created_at desc)`. A retention task prunes rows older than the configured window.

Migration: `database/migrations/0012_security_center.sql`, append-only and commented like `0009`.

### Events

**Emitted:** `security.scan.completed`, `security.finding.opened`, `security.finding.resolved`, `security.headers.updated`, `security.ip_rule.changed`, `security.lockout.triggered`, `security.limit.exceeded` (sampled, at most one per scope per window). **Consumed:** `user.login.failed` (lockout evaluation), `backup.completed` (so the backup-health check reads fresh data instead of polling).

Webhook relevance: `security.finding.opened` (critical/high) and `security.lockout.triggered` are the two an operator will subscribe to; both carry identifiers and severity, never the scanned content. Audit entries use the `security.*` namespace with actor, client IP and user agent.

### Acceptance criteria

- [ ] `crates/security` exists with posture checks, limiter policy and IP-rule evaluation, unit-tested.
- [ ] Migration `0012_security_center.sql` applies cleanly on fresh and populated databases.
- [ ] `/security` renders every check from the API with a truthful state; no check shows `pass` when unknown.
- [ ] "Run checks" records a new result set and the `Last checked` timestamps move.
- [ ] Header policy saves and the next API response carries the configured CSP/HSTS/Referrer-Policy values.
- [ ] Report-only mode sends `Content-Security-Policy-Report-Only`; enforce mode sends the enforcing header.
- [ ] Rate limits are enforced: exceeding a scope's window returns `429` with a `Retry-After` header.
- [ ] The limiter tester's verdict matches the real middleware decision for the same inputs.
- [ ] Five failed sign-ins for one account trigger the configured lockout and emit `security.lockout.triggered`.
- [ ] A locked account is listed with its unlock action, and unlocking restores sign-in.
- [ ] IP deny rules win over allow rules; adding a rule that would block the current client shows the warning.
- [ ] CIDR validation rejects malformed input (IPv4 and IPv6) with a field-level message.
- [ ] Ingesting a dependency report creates findings; re-ingesting the same report does not duplicate them.
- [ ] Acknowledge/ignore/mark fixed/reopen all persist; ignore without a reason is refused.
- [ ] CSV export of findings and security events matches the current filter.
- [ ] `/security/events` shows real sign-in, lockout, denial and settings-change entries.
- [ ] Secret inventory lists names and rotation age only; no value appears in HTML, JSON or export.
- [ ] CSRF protection rejects a cookie-authenticated mutation without a token.
- [ ] Every endpoint enforces its catalogue key; a forbidden call returns `403 permission_denied`.
- [ ] Walkthrough passes with zero high findings.

### QA plan

The walkthrough must visit `/security` and each sub-tab, click "Run checks", open a finding drawer, exercise acknowledge and ignore (with and without a reason), change the header mode, run the limiter tester, add a valid and an invalid IP rule, unlock a fixture-locked account, and export one CSV. Visual check should see: check rows legible at a glance (badge + text, not colour alone), the posture score ring rendered with a real number, no raw JSON visible anywhere, long CSP directive rows wrapping instead of overflowing, and the mobile pass stacking the two IP tables as cards.

### Slices

1. **Posture + findings** — schema, check registry, `/security` overview, findings list/detail and status transitions, audit entries. Done: the overview shows real states and a finding can be acknowledged, ignored with a reason and exported.
2. **Headers + CSRF** — header policy model, middleware application, CSP preview, CSRF token for cookie-authenticated mutations, `/security/headers`. Done: the configured headers appear on API responses and a mutation without the token is refused.
3. **Rate limiting + lockout** — Redis-backed limiter, scope table, tester, failed-attempt counting, lockout and unlock, `/security/sign-in-protection` and `/security/rate-limits`. Done: a scripted burst gets `429`, and five failed sign-ins lock the account until it is unlocked.
4. **IP access + events + inventory** — allow/deny evaluation, rules UI, security-event view, secret inventory projection, `security.finding.opened` webhook. Done: a denied CIDR cannot reach the API, the events screen shows the attempt, and the inventory shows rotation age without values.

### Risks / notes

- Never store or echo a secret value; the inventory is a projection over references. A leaked value in logs, audit payloads or CSV export is a release blocker.
- Rate limiting must fail open on a Redis outage but log it — a limiter that takes the platform down is worse than no limiter; the setting exposes this choice explicitly.
- Lockout can be weaponised against a known account: count per IP as well as per account, and keep the unlock path (including a CLI path) working when the panel is unreachable.
- Config-based checks are only as truthful as their inputs: a check that cannot verify something must report `unknown` with the reason, never `pass`.
- CSRF tokens must not break the public renderer or webhook intake; those routes are exempt by design and the exemption list lives in one place.
