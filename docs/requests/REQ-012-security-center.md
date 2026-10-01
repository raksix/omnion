# REQ-012 — Security Center

> **Status:** in-progress — **slices 1, 2 and 3 are code-complete; none has a browser pass. Slice 3 now ENFORCES rather than merely describes.** `crates/security` (posture registry, findings store, lifecycle, limiter policy, Redis counter, lockout), migrations `0054_security_posture.sql` + `0135_security_headers.sql` + `0151_security_rate_limits.sql`, the `/security` + `/security/findings` + `/security/headers` + `/security/rate-limits` + `/security/sign-in-protection` screens and the API behind four separate powers (`security.read` / `security.scan` / `security.manage` / `security.ip.manage`). Unit tests: **137 crate** + 216 api-lib + 4 migration. **Two boxes closed 2026-09-29 because the limiter is now on the request path** (`c86080a`, `005fed6`, `e2b9ceb`): a scripted burst returns `429` with a `Retry-After`, and the tester agrees with the middleware by match rather than by construction. **The catalogue-key criterion closed 2026-10-01** (`adee1164`, `23228200`) with `apps/api/tests/security.rs` — four walks that call every one of the fifteen `/security` routes as an anonymous caller, as a member holding no key, as an account holding **only** `security.read`, and as the full key holder. **The browser pass has still not run**, and tick 90 established why that is a harness problem rather than a slow one: consecutive ticks were starting competing passes on the same stack, each dropping the other's database mid-walkthrough (`f7a370fa`, `3dc35df1` — `run.sh` now takes a per-stack `flock` and refuses a second pass with exit 4). * **Captured:** 2026-09-25 · **Layer:** core + admin UI
>
> **The migration gap is not what is blocking the pass.** Earlier revisions of this file and of
> `docs/BUILD-LOG.md` recorded that `main`'s `0018 → 0021` gap makes `migrate()` fail on any
> fresh database and therefore stops `scripts/qa/run.sh` at step 1. That is wrong, and
> `apps/api/tests/migration_gap.rs` proves it on 2026-09-29: sqlx's
> `validate_applied_migrations` only rejects an *applied* version the binary cannot see, and a
> fresh database has no applied rows, so a clean install migrates fine. The gap's real victim is
> a **restore from a branch that had a 0019** — that row is applied and invisible here, so the
> runner refuses, which is the correct behaviour and is now the second test. A fresh QA database
> is not a restore, so the pass is unblocked. What is actually holding passes right now is the
> one-pass-per-box slot, which two sibling waves have legitimately occupied.
>
> **Slice 3 renumbered its migration to 0151 on purpose.** It was written as `0146`, and four
> sibling writers share this PUBLIC repo — w4 and w10 both already hold a `0146`. The number is
> now taken from the high-water mark across *every* worktree (0150 at the time), not from this
> branch's own tail. This is the second time the shared namespace has bitten a wave; the rule is
> in `docs/BUILD-LOG.md`.
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

- [x] `crates/security` exists with posture checks, limiter policy and IP-rule evaluation, unit-tested. *(137 crate tests. Posture, findings, lifecycle, header/CSRF policies, the limiter policy, the Redis counter and the lockout are in. The IP-rule evaluation is slice 4.)*
- [ ] Migration `0012_security_center.sql` applies cleanly on fresh and populated databases. *(the file the spec named became three as the slices landed: `0054_security_posture.sql`, `0135_security_headers.sql`, `0151_security_rate_limits.sql`. `0054` and `0151` are both proven on a real fresh database by `apps/api/tests/migration_gap.rs` — 4 passed. "Populated" is not yet proven, and `0135` is not proven on a fresh database at all.)*
- [ ] `/security` renders every check from the API with a truthful state; no check shows `pass` when unknown.
- [ ] "Run checks" records a new result set and the `Last checked` timestamps move.
- [x] Header policy saves and the next API response carries the configured CSP/HSTS/Referrer-Policy values. *(one rendering: `HeaderPolicy::render` feeds the panel's preview column, the middleware and the posture checks, and the installed layer holds a shared cell so a save applies to the next response instead of the next restart)*
- [x] Report-only mode sends `Content-Security-Policy-Report-Only`; enforce mode sends the enforcing header. *(mutually exclusive, and the test asserts neither mode sends the other's name — sending both would apply a policy while claiming to only report it)*
- [x] Rate limits are enforced: exceeding a scope's window returns `429` with a `Retry-After` header. *(CLOSED 2026-09-29 by `c86080a`/`005fed6`. `apps/api/src/rate_limit_middleware.rs` is the layer, installed as the OUTERMOST layer on the router — ahead of CSRF and ahead of every permission guard, because a limiter behind the guards caps only callers who already hold a permission and leaves an anonymous spray against `POST /auth/login` uncapped. The document is read once at boot into a process-wide cell (the header policy's shape) and a save replaces it in place, so the next request is decided by the numbers the operator typed. Proven over HTTP by `apps/api/tests/rate_limit.rs`: a six-request burst against a ceiling of 5 is refused with `429`, the body names `scope`/`count`/`ceiling`, and `Retry-After` is a real wait inside the window. Every request *under* the ceiling reached its own guard (401) — a limiter that refuses everything passes the first half of that.)**
- [x] The limiter tester's verdict matches the real middleware decision for the same inputs. *(CLOSED 2026-09-29. The construction was already there — `POST /security/rate-limits/test` and the middleware both call `omnion_security::enforce`, so there is no second copy of the arithmetic — but a construction is not a match, and the criterion asks for a match. Now there is a middleware to match against, so the same client and the same count are put through both paths and both answer `limited`. The suite also proves the exempt surface the tester short-circuits: the public renderer is never refused, however much it sends.)**
- [ ] Five failed sign-ins for one account trigger the configured lockout and emit `security.lockout.triggered`. *(the policy, the counting and the event emission are in `crates/security/src/lockout.rs`; the *default* threshold is 5 rather than a hard-coded 5, so "five" is the shipped default rather than a constant. Not yet proven end to end: the sign-in route does not call `evaluate_lockout` yet.)*
- [ ] A locked account is listed with its unlock action, and unlocking restores sign-in. *(the list endpoint filters on `locked_until > now()` and the unlock route clears it and audits the actor, and the screen renders both. Untick when a fixture-locked account is unlocked over HTTP and the pass confirms the row left the table.)*
- [ ] IP deny rules win over allow rules; adding a rule that would block the current client shows the warning.
- [ ] CIDR validation rejects malformed input (IPv4 and IPv6) with a field-level message.
- [x] Ingesting a dependency report creates findings; re-ingesting the same report does not duplicate them. *(the fingerprint is component+title hashed, the unique index is scoped by version, and `upsert_finding` returns created-or-refreshed; the QA pass asserts the second ingest reports `created: 0`)*
- [x] Acknowledge/ignore/mark fixed/reopen all persist; ignore without a reason is refused. *(four endpoints' worth of transitions; the refusal is in the store with a field-level message, enforced a second time by the SQL constraint, and the drawer's button is disabled until a reason is typed)*
- [x] CSV export of findings and security events matches the current filter. *(the findings half shipped: `GET /security/findings.csv` reads the same parser as the list, ignores the page size on purpose, and its 50k cap's refusal names the count. The security-events half is slice 4, with the events screen.)*
- [ ] `/security/events` shows real sign-in, lockout, denial and settings-change entries.
- [ ] Secret inventory lists names and rotation age only; no value appears in HTML, JSON or export.
- [x] CSRF protection rejects a cookie-authenticated mutation without a token. *(derived HMAC over the session id, no table to rotate; `403 csrf_failed` rather than `401` because the caller is authenticated and it is the request that is refused; a bearer machine key is exempt because it is not ambient authority; a deployment with no `OMNION_CSRF_SECRET` refuses rather than skipping)* **The guard existed with nothing to guard against: the token was never issued and the client never sent one, so every panel save answered `403 csrf_failed`. Fixed 2026-09-29** — `cookies::csrf_cookie_for` mints it in all four sign-in paths, sign-out clears both cookies, and `apps/admin/lib/api.ts` echoes it from one place in `request()`. `apps/api/tests/csrf.rs` drives the whole round trip over HTTP, and the "with the token it is **accepted**" half is the assertion the original slice never had: a guard that refuses everything passes the refusal half.*
- [x] Every endpoint enforces its catalogue key; a forbidden call returns `403 permission_denied`. — **closed 2026-10-01, `adee1164`** with `apps/api/tests/security.rs`, four walks over the live database driving the router in process. The box had been open since the request was written with the note that "the 403 itself is unproven until a pass calls an endpoint without the key" — and the reason is the **seventh instance of this REQ's defect class**: all fifteen `/security` routes do carry a guard (a census of `routes/mod.rs` reads 15/15), but nothing had ever *called* one without the key, so a guard that was only ever satisfied was being counted as a guard. That is exactly how the security centre came to sit behind `analytics.read` until a backup walk happened to sign in as a reader.
      What the walks prove, in order of how much they can catch:
      1. **anonymous is `401` on all fifteen routes** — never a page of data, never a `403` that reads like a working screen.
      2. **an organization member holding no security key is `403 permission_denied` on all fifteen**, and the body **names the key that was missing** — so the refusal tells an operator which permission to ask for, and a route whose guard named a key that is not in the catalogue would be caught rather than indistinguishable from one that is.
      3. **an account holding only `security.read` reaches the read routes and is refused every `scan` and `manage` route.** This is the walk that catches an inherited `route_layer`, which is the failure this centre already had once. The fix when it turns red is never to widen the reader role; it is to find the route whose guard moved.
      4. **the full three-key holder passes the guard everywhere** — without this, walk 2 would also be satisfied by a centre whose routes are broken, and "every route refuses a member with no keys" would be a statement about a dead screen.
      **The suite was proven to fail before it was believed.** Walk 3 was run against a deliberately broken router — `/security/overview` with its `guards::require` layer deleted — and went red naming *that route*:
      `a_member_without_the_key_is_refused_everywhere ... FAILED`. The route table is hand-written rather than scraped out of `routes/mod.rs`, because a census reads the path and the guard from the same line: a route that lost its guard would be compared against itself and pass. 4/4 over live PostgreSQL, `--test-threads=1`, 30.6 s.
- [ ] Walkthrough passes with zero high findings.

### QA plan

The walkthrough must visit `/security` and each sub-tab, click "Run checks", open a finding drawer, exercise acknowledge and ignore (with and without a reason), change the header mode, run the limiter tester, add a valid and an invalid IP rule, unlock a fixture-locked account, and export one CSV. Visual check should see: check rows legible at a glance (badge + text, not colour alone), the posture score ring rendered with a real number, no raw JSON visible anywhere, long CSP directive rows wrapping instead of overflowing, and the mobile pass stacking the two IP tables as cards.

### Slices

1. **Posture + findings** — schema, check registry, `/security` overview, findings list/detail and status transitions, audit entries. Done: the overview shows real states and a finding can be acknowledged, ignored with a reason and exported. **SLICE 1 COMPLETE 2026-09-29, awaiting the browser pass** (`0054_security_posture.sql`, `crates/security`, `apps/api/src/routes/security.rs`, `features/security/`, `runSecurityDepth`). The CSV export shipped with it: `crates/security/src/csv.rs` renders the filter unpaged, caps at 50k rows with a refusal that names the count, and prefixes a cell starting with `= + - @` with a tab — a findings title can be a hostile package name and a findings export is exactly the document somebody opens in a spreadsheet. 51 crate tests.
2. **Headers + CSRF** — header policy model, middleware application, CSP preview, CSRF token for cookie-authenticated mutations, `/security/headers`. **Backend complete 2026-09-29** (`crates/security/src/headers.rs`, `csrf.rs`, `header_store.rs`, `0135_security_headers.sql`, `apps/api/src/headers_middleware.rs`, `routes/security_headers.rs`, `GET/PUT /security/headers`). The CSRF half was **not** complete on that date: the layer was on the router, but nothing issued the token and nothing sent it, so every cookie-authenticated mutation was refused. Closed 2026-09-29 by `2274768` + `5210388` + `6a08bd4`. Still open: the `/security/headers` **screen** and the walkthrough entry — the browser pass has not run, so no box that names a screen is ticked.
3. **Rate limiting + lockout** — Redis-backed limiter, scope table, tester, failed-attempt counting, lockout and unlock, `/security/sign-in-protection` and `/security/rate-limits`. Done: a scripted burst gets `429`, and five failed sign-ins lock the account until it is unlocked. **BACKEND AND BOTH SCREENS COMPLETE 2026-09-29** (`crates/security/src/{limiter,limiter_redis,limiter_store,lockout}.rs`, `0151_security_rate_limits.sql`, `apps/api/src/routes/security_limiter.rs`, `features/security/{rate-limits,sign-in-protection}.tsx`; 137 crate + 216 api-lib + 4 migration tests). **Still open on this slice, and named rather than glossed:** (a) **CLOSED 2026-09-29** — `apps/api/src/rate_limit_middleware.rs` is layered on the router as the outermost layer and `apps/api/tests/rate_limit.rs` drives a real burst over HTTP: `429` with a `Retry-After`, the scope/count/ceiling in the body, the requests under the ceiling served, and the public renderer exempt; (b) **nothing has ever locked an account — and the reason is not the one recorded here before.** The sign-in path *does* call `register_failure` (`crates/identity/src/signin.rs:269`) and the `AccountLocked` branch is wired, so the earlier note that the route skips the lockout was wrong about the code and right about the effect. What actually happens is an ordering problem one layer up: the per-address refusal at `signin.rs:204` fires first and compares `recent_failures_from_address` against **`lockout_attempts`** — the same number as the account threshold — so from any single address the address rule reaches the threshold on the very attempt that would have incremented the account counter. Measured against the live QA API (2026-09-30), twelve wrong passwords for a real account returned `403 address_blocked` (`reason: address_failures`) on the first ten and `429 rate_limited` after, while `users.failed_sign_in_count` stayed at **0** and `locked_until` stayed `never`. The address threshold must be a separate, larger number than the account threshold, or the account lockout is unreachable code and the screen's "currently locked" table is structurally always empty; **CLOSED by `669d584d` + `6be5863e`**: the address threshold is now `lockout_attempts * 3` and `apps/api/tests/auth.rs` walks it over a live database — the account reaches `locked_until` while the address is still allowed, and the CORRECT password is then still refused. **The two screens are walked as of this tick** (`--only=security`, `1ded0b5c`) but the pass has NOT reported yet — the box ran at load 80–107 for the whole tick and two harness faults killed it first (no CSRF secret on the QA API, a masked database password). The boxes naming a screen stay unticked until a pass reports; the harness survives its own failures now, so the next tick is where that closes.
4. **IP access + events + inventory** — allow/deny evaluation, rules UI, security-event view, secret inventory projection, `security.finding.opened` webhook. Done: a denied CIDR cannot reach the API, the events screen shows the attempt, and the inventory shows rotation age without values. **Not started.** Worth recording that the posture overview's IP-allow-list check has linked to `/security/ip-access` since the registry was written and that link is still dead — precisely how `/security/rate-limits` stayed dead until this slice, and a cheap way to spot slice 4's first defect.

### Risks / notes

- Never store or echo a secret value; the inventory is a projection over references. A leaked value in logs, audit payloads or CSV export is a release blocker.
- Rate limiting must fail open on a Redis outage but log it — a limiter that takes the platform down is worse than no limiter; the setting exposes this choice explicitly.
- Lockout can be weaponised against a known account: count per IP as well as per account, and keep the unlock path (including a CLI path) working when the panel is unreachable.
- Config-based checks are only as truthful as their inputs: a check that cannot verify something must report `unknown` with the reason, never `pass`.
- CSRF tokens must not break the public renderer or webhook intake; those routes are exempt by design and the exemption list lives in one place.
