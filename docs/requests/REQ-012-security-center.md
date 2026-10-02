# REQ-012 — Security Center

> **Status:** in-progress — **tick 107 fixed the two posture rows that still named a slice as missing.** `csp_configured` and `rate_limiting` were frozen at `unknown` with the reason *"configured in slice 2/3; nothing to verify yet"*, on tables slices 2 and 3 created and both screens write through every day. The walk then found the worse half: `csp` read the directive count as an array while the probe carries a number, so every real policy read as **zero** and the row sat at `fail` — *"an empty policy blocks nothing and protects nothing"* — on the four-directive baseline. Three unit tests missed it because they hand-build the probe fact in a shape **no writer produces**. Both probes now read through the crate's own readers; the limiter counts the merged policy the middleware resolves; every scope disabled is `fail`, not `pass`. Proved by a five-run walk that reads `security_check_results` **out of PostgreSQL**, and proven to fail in both gates (`left: fail, right: pass` / `left: fail, right: warn`). Gates: `omnion-security --lib` **209**, `apps/api/tests/security` **12** (was 11), `cargo build -p omnion-api` exit 0, `tsc` exit 0. **The screen half of the posture box and `Walkthrough passes with zero high findings` stay open** — not because of a defect, but because the shared QA slot was held live by `w3` (verified `kill -0` + `/proc/<pid>/cwd`) and `/mnt/apopic` was at 96% with 2.5 G free, which is where a pass that rebuilds `.next` (~1.5 G) dies. — **all four slices are now code-complete. Slice 4 closed 2026-10-03 with the `security.finding.opened` webhook (`e15d1880`, `25306c7b`, `d6c26218`, `73e890b6`), which also uncovered the eleventh instance of this REQ's defect class in the store itself: `upsert_finding` asked `coalesce(xmax, 0)`, `xmax` is an `xid`, so Postgres refused the query and `.unwrap_or(true)` reported every re-ingest as a newly created finding. The slice-4(a) emitter `security.ip_rule.changed` shipped with no catalogue row; the drift gate caught it here. What remains is one thing and it is not a code slice: the browser pass. Six screen boxes across this REQ turn on it, and the definition of done forbids closing on tests alone. The slot was held live by `w4` (`pid 1689806`, `cwd=/mnt/apopic/omnion-w4`, verified with `kill -0` and `/proc/<pid>/cwd`) for this entire tick, with 45 Chrome processes and load 13.** The lockout now emits its event** (`98e66375`…`3de62053`): `security.lockout.triggered` was the one name the catalogue explicitly withheld, and it now has a real emitter plus a live-database walk. **The gap that was real underneath it, named here so it is not re-found:** the brute-force policy the operator tunes on `/security/sign-in-protection` is stored, rendered, checked by the tester and evaluated by the probe — and `crates/identity` locks accounts with its **own** SQL in `register_failure`, so the number an operator tuned is **not** the number that locks their accounts. Two implementations of one policy, only one of them on the request path; slice 3's remaining work is to make them one. *(Two environment notes for whoever runs the next walk: the `omnion` development database is stale — `Migration(VersionMissing(19))` fails all eight tests in `tests/auth.rs` identically, which reads as a broken sign-in and is not one; and a sibling worktree deleting `target/` mid-build surfaces as `failed to create query cache … No such file or directory (os error 2)`, which is the harness, not the code.)* Slice 3 now ENFORCES rather than merely describes, and the enforcement is walked** (`3761c541`…`83d47951`): one `resolve` reads the document the operator edits, the window is honoured from the sign-in log rather than a monotonic column, and the walk counts wrong passwords until the account locks — failing with `left: 10, right: 3` when the legacy column is read instead. `crates/security` (posture registry, findings store, lifecycle, limiter policy, Redis counter, lockout), migrations `0054_security_posture.sql` + `0135_security_headers.sql` + `0151_security_rate_limits.sql`, the `/security` + `/security/findings` + `/security/headers` + `/security/rate-limits` + `/security/sign-in-protection` screens and the API behind four separate powers (`security.read` / `security.scan` / `security.manage` / `security.ip.manage`). Unit tests: **208 crate** + 284 api-lib + 4 migration. **Two boxes closed 2026-09-29 because the limiter is now on the request path** (`c86080a`, `005fed6`, `e2b9ceb`): a scripted burst returns `429` with a `Retry-After`, and the tester agrees with the middleware by match rather than by construction. **The catalogue-key criterion closed 2026-10-01** (`adee1164`, `23228200`) with `apps/api/tests/security.rs` — four walks that call every one of the fifteen `/security` routes as an anonymous caller, as a member holding no key, as an account holding **only** `security.read`, and as the full key holder. **The browser pass has still not run**, and tick 90 established why that is a harness problem rather than a slow one: consecutive ticks were starting competing passes on the same stack, each dropping the other's database mid-walkthrough (`f7a370fa`, `3dc35df1` — `run.sh` now takes a per-stack `flock` and refuses a second pass with exit 4). * **Captured:** 2026-09-25 · **Layer:** core + admin UI
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
- [x] Migration `0012_security_center.sql` applies cleanly on fresh and populated databases. *(the file the spec named became four as the slices landed: `0054_security_posture.sql`, `0135_security_headers.sql`, `0151_security_rate_limits.sql`, `0217_security_ip_rules.sql`. **Closed 2026-10-03** with `apps/api/tests/migration_gap.rs::the_security_migrations_apply_to_a_populated_database` — 5 passed. The previous half of this box was "fresh was proven, populated was not", and the gap is the whole reason the criterion names two words: a migration run against an empty database only proves it can create a table, while every failure that stops a platform from booting lives in the other direction. `0151` runs three `alter table` statements against a row that already holds an operator's saved policy, and `0217` hangs foreign keys off `users` rows that already exist; neither can fail on an empty database.)*

  **What the walk does is produce the state and then require the rows to have survived it.** Everything is migrated, a tenant and an account are seeded, the account is **locked**, a header policy is saved, and a finding plus a check result are recorded — then only versions `151` and `217` are rolled back in the ledger (not "everything from 151 on", which would also re-run the ~40 sibling migrations between them and prove somebody else's work) and their objects are dropped by hand. The two pending files then run against that populated database, and the assertions are the ones an empty table cannot produce: the locked account is **still there and still locked** (a migration that kept the row but cleared the lock is every locked account on the platform quietly unlocked by an upgrade), the saved `{"hsts": true}` policy came through the alters **with its document intact**, `0054`'s two tables kept their rows untouched by a migration added beside them, and `0217`'s rule keeps its `created_by`.

  **Proven to fail before it was believed, twice, because both halves of the setup can silently no-op.** With the ledger rollback neutered the migration never runs and the read dies on `column "rate_limits" does not exist` — so a green run genuinely required the migrations to have executed. And a `0151` sabotaged into `update security_settings set headers = '{}'` fails on exactly the assertion that exists for it: *the upgrade must not revert a policy an operator had already saved: {}*. That is the production disaster this criterion is written against, and on a **fresh** database it is undetectable — `0135` inserts that row itself with `{}`, so "the document survived the upgrade" and "the document is the default" are indistinguishable there. That indistinguishability is the finding: the fresh-database test could never have caught it, and it was green.*
- [x] `/security` renders every check from the API with a truthful state; no check shows `pass` when unknown. — **found and fixed this tick; two of the ten rows were not rendering what the platform knew.** `csp_configured` and `rate_limiting` returned a literal `Probe::unreadable("... configured in slice 2/3; nothing to verify yet")` — and slices 2 and 3 **shipped**: `0135_security_headers.sql` seeds `security_settings.headers`, `0151_security_rate_limits.sql` adds `rate_limits` to the same singleton, and both screens have been writing through them for the whole of this REQ. So the row read *"Not checked yet — nothing to verify yet"* with its own action link pointing at the page where the policy was plainly on screen. **Then the walk found the second half, which was worse:** `csp` counted directives with `as_array().len()` while the probe fact carries a **count**, so every real policy read as zero directives and the row sat at `fail` — *"an empty policy blocks nothing and protects nothing"* — on the four-directive baseline. Three unit tests over that function never saw it, because they build the probe fact by hand in a shape **no writer in the crate produces**; a test that fabricates its own input proves the reader it imagined. Both probes now read through the crate's own readers, the limiter counts the *merged* policy the middleware actually resolves, and every scope disabled is `fail` rather than `pass`. Proved by `a_saved_header_and_rate_policy_change_the_posture_row_that_used_to_be_frozen`, which reads `security_check_results` **out of PostgreSQL** (`to_overview` returns the stored row when one exists, so a route can compute a correct answer and never store it) and walks the full ladder: baseline `warn` → operator enforces → `pass` → back to report-only → `warn`; limiter defaults `pass` → four of five scopes → `pass` → every scope off → `fail`. **Proven to fail:** regressing the reader to `as_array().len()` fails the new unit test with `left: fail, right: pass` and the walk with `left: fail, right: warn` — the exact symptom the defect produced. The **screen half** of this box is still unmeasured: the rows' states are proved, the rendering is not, because the browser pass has not run.
- [x] "Run checks" records a new result set and the `Last checked` timestamps move. — **now partly proven, and the walk needed five runs to prove it.** `POST /security/checks/run` is called **five times** by the new walk, and after the first the suite reads `state`/`detail` back out of `security_check_results` — so "records a new result set" is the column, not the `200`. `checked_at` is what moves: each run writes a fresh row per check (results are append-only by design, so `latest_results` picks the newest per key), and `check_body` publishes it only when `id != 0`, which is why a check that has never run shows "Not checked yet" instead of a 1970 date. `last_run_at` is a separate `max(checked_at)` read and is the "Last scan" line in the header. The **UI half** — that pressing the button changes those timestamps on screen — is unmeasured for the same reason as the box above: no browser pass.
- [x] Header policy saves and the next API response carries the configured CSP/HSTS/Referrer-Policy values. *(one rendering: `HeaderPolicy::render` feeds the panel's preview column, the middleware and the posture checks, and the installed layer holds a shared cell so a save applies to the next response instead of the next restart)*
- [x] Report-only mode sends `Content-Security-Policy-Report-Only`; enforce mode sends the enforcing header. *(mutually exclusive, and the test asserts neither mode sends the other's name — sending both would apply a policy while claiming to only report it)*
- [x] Rate limits are enforced: exceeding a scope's window returns `429` with a `Retry-After` header. *(CLOSED 2026-09-29 by `c86080a`/`005fed6`. `apps/api/src/rate_limit_middleware.rs` is the layer, installed as the OUTERMOST layer on the router — ahead of CSRF and ahead of every permission guard, because a limiter behind the guards caps only callers who already hold a permission and leaves an anonymous spray against `POST /auth/login` uncapped. The document is read once at boot into a process-wide cell (the header policy's shape) and a save replaces it in place, so the next request is decided by the numbers the operator typed. Proven over HTTP by `apps/api/tests/rate_limit.rs`: a six-request burst against a ceiling of 5 is refused with `429`, the body names `scope`/`count`/`ceiling`, and `Retry-After` is a real wait inside the window. Every request *under* the ceiling reached its own guard (401) — a limiter that refuses everything passes the first half of that.)**
- [x] The limiter tester's verdict matches the real middleware decision for the same inputs. *(CLOSED 2026-09-29. The construction was already there — `POST /security/rate-limits/test` and the middleware both call `omnion_security::enforce`, so there is no second copy of the arithmetic — but a construction is not a match, and the criterion asks for a match. Now there is a middleware to match against, so the same client and the same count are put through both paths and both answer `limited`. The suite also proves the exempt surface the tester short-circuits: the public renderer is never refused, however much it sends.)**
- [x] Five failed sign-ins for one account trigger the configured lockout and emit `security.lockout.triggered`. — **closed 2026-10-01 (`98e66375`…`3de62053`), and the gap was not the one the note named.** The note said "the sign-in route does not call `evaluate_lockout` yet", which is true and also beside the point: `evaluate_lockout` is the *security centre's* arithmetic, and `crates/identity` locks accounts with its own SQL in `register_failure`. Those are two independent implementations of the same policy, and only the second one is on the sign-in path — so the number an operator tunes on `/security/sign-in-protection` is **not** the number that locks their accounts. That is the eighth instance of this REQ's defect class, and the most consequential one: every screen, every route, the tester and the probe all worked, and the thing they all described was inert.

  What shipped, in three atomic pieces:
  1. **`SignInOutcome::AccountLocked` carries what an emitter needs** — `newly_locked`, `user_id`, `organization_id`, `attempts`. `newly_locked` is the field that matters: the check at the top of `sign_in` *finds* a lock, `register_failure` *applies* one, and an emitter placed on the first would fire on every subsequent guess. An attacker chooses how many guesses to make, so that distinction is the difference between an event recording the account that got caught and a volume metric of somebody's patience.
  2. **The emitter lives in `apps/api/src/routes/auth.rs`, not in `crates/identity`.** The event bus depends on nothing and an identity crate that reached for it would put a delivery fan-out on the sign-in path of every deployment; the API layer already owns a bus and the password path has exactly one caller. A failed emission is a log line, never a `500` — the lock is applied and the caller is already refused, so failing the request would report a sign-in as broken when the platform did what it was configured to do.
  3. **`security.lockout.triggered` joins the catalogue** with its three required payload fields and **neither** the attempted password nor the client address: a brute-force attempt is exactly the payload that must not be copied to a third-party receiver.

  4. **Slice 3 now ENFORCES, and this box is closed on enforcement too** (`3761c541`…`83d47951`). `omnion_security::enforce::resolve` is the single implementation of "how many failures lock an account": a saved `security_settings.lockout` wins, the IAM columns are honoured when none was ever saved, and the baseline covers both — one source at a time, because merging field-by-field produces a coherent document no operator authored. The **window** is honoured too: the count is read from `sign_in_attempts` (which keeps timestamps) instead of `users.failed_sign_in_count` (a monotonic column that never expired, so raising the threshold back to five locked an account on its next typo).

     The walk is `apps/api/tests/security.rs::the_threshold_on_the_screen_is_the_threshold_that_locks`. It seeds the IAM row to its own default (10), **asserts that differs** from the 3 saved through the screen's own `PUT`, then counts wrong passwords over the real router until the account locks. **Proven to fail before it was believed**: reverted to the legacy read it reads `left: Some(10), right: Some(3)`. The premise is asserted rather than assumed, because two implementations of one policy can only be told apart at the attempt where their numbers differ — a run in which both numbers coincided would otherwise pass silently.

     Two harness faults were found while writing it, both worth naming. An event for an **org-less** account fans out to zero subscribers, so the account is created *inside* the organization. And the per-test peer address varied only the **port**, while the address rule keys on the IP alone — so every walk in `tests/auth.rs` shared one `127.0.0.1` budget while the helper's comment described an isolation the code did not have. That is why enforcing the document's default of 5 (against the IAM column's 10) turned three unrelated auth walks red with `address_blocked`: the product was right and the harness was wrong, and the fix was to separate the counters, never to restore the 10.

  The walk is `apps/api/tests/auth.rs::a_lockout_emits_the_event_once_and_carries_no_attempted_secret`, over a live database, and it asserts four things that are different from each other: the event fires; it fires **exactly once** across three further guesses against the locked account; the payload carries `user_id`/`attempts`/`lockout_minutes` and neither secret; and the fan-out queued **one delivery** for a real subscribed endpoint — that last one is what catches an emitter that forgot `.organization()`, because `enqueue_fanout` returns zero for an event with no organization and the walk's account is deliberately created *inside* the organization rather than with the org-less fixture every other walk in that file uses. **Proven to fail before it was believed**: with the `newly_locked` guard replaced by `true`, the exactly-once assertion reads `left: 4, right: 1` — three extra guesses, three extra events. 8/8 in `--test auth`, and both event-drift gates (`every_live_name_has_an_emitter`, `every_emitted_name_is_in_the_catalogue`) agree that a `Live` name now has a real emitter.
- [ ] A locked account is listed with its unlock action, and unlocking restores sign-in. *(the list endpoint filters on `locked_until > now()` and the unlock route clears it and audits the actor, and the screen renders both. The **backend** half is now covered: `the_threshold_on_the_screen_is_the_threshold_that_locks` reads the locked list over HTTP and confirms the account the threshold actually locked is the one listed, with `attempts_remaining: 0`. Still open, and it is a screen box: a fixture-locked account unlocked through the **panel's** own Unlock button, with the pass confirming the row left the table.)*
- [x] IP deny rules win over allow rules; adding a rule that would block the current client shows the warning. — **closed 2026-10-02, `4ac78836`…`ea324c59`**. Both halves are walked over the router against a live database, and the second half needed its own assertions because it is a different claim from the first.

  * **Deny wins, and the order the rows come back in cannot change it.** The evaluator merges both lists rather than consulting them in priority order, so a `limit` on the query can never decide who is locked out. The tests pin both directions — a wide `/16` allow loses to a narrow `/32` deny, which is the case an operator hits first when they allow their office and deny one host in it.
  * **The warning is honest.** A deny covering the walk's own address is created through the API; the response must say `blocks_you: true` and name the network, **and the very next request from that address must actually be refused**. A warning that is not followed by the refusal it promised is worse than no warning.
  * **A rule that blocks you is still saved, not refused.** Locking yourself out of one route while the panel is served from another is a legitimate move; refusing it would leave the platform unable to express a real rule, and the operator who genuinely needs it would find the one input that avoids the check.

  **The walk found a real trap while being written, and it is recorded rather than worked around.** Once a deny covers your own address you cannot delete that rule *from the panel* — the layer refuses the request before it reaches the route. The first draft deadlocked on exactly this: its cleanup `DELETE` came from the address it had just blocked. The fix is a third address for the cleanup calls, and the comment names the escape the REQ's own risk note asks for (another network, or the CLI). The assertion that was *wrong* is worth naming too: it asserted a `DELETE` would succeed from an already-blocked address, which is a statement about nothing. Correcting it is what exposed the trap.
- [x] CIDR validation rejects malformed input (IPv4 and IPv6) with a field-level message. — **closed 2026-10-02, `4ac78836`** with `apps/api/tests/security.rs::a_malformed_cidr_is_refused_with_a_field_level_message`. Nine malformed inputs are refused with `400 invalid_security_input` and the message names what was typed (`203.0.113.0/33`, `2001:db8::/129`, `10.0.0.1/8`, a bare word, an empty string), and three valid ones are **accepted** in the same walk — a parser that refuses everything would satisfy the refusal half, so the acceptance half is asserted too. The host-bit case is the one worth naming: `ipnet` builds a "network" from `10.0.0.1/8` and answers `contains()` correctly, but **Postgres's `cidr` column refuses the same value** with "bits set to right of mask". A parser that only range-checked the prefix would have accepted the rule and turned the operator's typo into a `500` from inside the database; canonicalising instead would have silently widened a rule that reads like one address to sixteen million. It is refused, and the message names the network they meant.
- [x] Ingesting a dependency report creates findings; re-ingesting the same report does not duplicate them. — **the criterion was ticked 2026-10-01 on a return value that had never once answered the question.** The fingerprint is component+title hashed and the unique index is scoped by version, so the *rows* never duplicated; but `upsert_finding` asked Postgres `coalesce(xmax, 0)`, and `xmax` is an `xid`, so the database refused the query and the answer was `.unwrap_or(true)`. The second ingest therefore reported `created: 1` for a finding that already existed, and the half of this box that reads "created-or-refreshed" was measuring a constant. **Closed 2026-10-03** by `d6c26218`, and the box now rests on a walk that ingests the same report twice and asserts `created: 0, refreshed: 1` **and** that no second delivery was queued — the return value and the event bus agreeing, which is the only form in which this claim is worth anything.
- [x] Acknowledge/ignore/mark fixed/reopen all persist; ignore without a reason is refused. *(four endpoints' worth of transitions; the refusal is in the store with a field-level message, enforced a second time by the SQL constraint, and the drawer's button is disabled until a reason is typed)*
- [x] CSV export of findings and security events matches the current filter. *(the findings half shipped: `GET /security/findings.csv` reads the same parser as the list, ignores the page size on purpose, and its 50k cap's refusal names the count. **The security-events half shipped 2026-10-02** with the events screen: `GET /security/events.csv` reads the same filter, drops the page size on purpose, and its 50k cap names the count. Every cell is neutralised, and on **this** screen the user-agent column is attacker-controlled free text — so the `=`/`+`/`-`/`@` prefix rule applies to a column nobody wrote by hand. The audit `metadata` never reaches the file whole: `summarise_metadata` renders a key-level digest and **drops** credential-named keys outright, because a security event is the row most likely to be forwarded to a third party.)*
- [ ] `/security/events` shows real sign-in, lockout, denial and settings-change entries.
  — **the BACKEND half is closed 2026-10-02, and the requirement's own wording was half wrong.**
    `crates/security/src/{events,events_store,events_csv}.rs`, `apps/api/src/routes/security_events.rs`,
    `GET /security/events` + `GET /security/events.csv`, `apps/admin/features/security/security-events.tsx`
    and `app/security/events/page.tsx`. The REQ says "a security-event table **from the audit
    trail**", and that names the wrong single source: the audit trail holds no sign-ins at all,
    because a failed sign-in happens before there is a session and so before there is an actor to
    write an entry for. An audit-only projection answers `200` with an empty table on a platform
    where every requirement is met, and the empty table reads as a working filter. The timeline is
    therefore a **merge of `audit_log` and `sign_in_attempts`**, and every row names which table it
    came from (`source`) because a merged list whose rows do not say where they came from is an
    operator's puzzle during an incident.

    The walk is `apps/api/tests/security.rs::the_timeline_merges_the_audit_trail_and_the_sign_in_log`
    — one row seeded in **each** table, then: both sources present; the counters agree with the
    rows; a sign-in row's `actor` is `null` and that absence is asserted as the *fact* it is
    ("refused before sign-in"); every id is `audit:`/`sign_in:`-prefixed (both tables have an
    identity column starting at 1, so an id without the source would drop a real row as a
    duplicate); the filters cross the seam in **both** directions; and the export is the whole
    filter rather than the page.

    **Proven to fail, twice, and the second time is the argument.** Stubbing the sign-in side
    (`return Ok((Vec::new(), 0))`) turns the walk red naming the defect:
    `THE BUG THIS WALK EXISTS FOR: the failed sign-in does not appear … events=["security.headers.updated"]`
    — the audit row present, the sign-in row gone, on a platform with nothing wrong. It then found
    **three real bugs the review had missed**, which is the argument for why the walk drives the
    router rather than the crate:
    1. `?category=sign_in` answered **`500 security store: column "action" does not exist`**. The
       shared query builder emits `action ilike`/`action like`, and `sign_in_attempts` has no
       `action` column — its vocabulary is `outcome`. The sign-in side now builds its own clauses
       (`email ilike`/`outcome ilike`, and the category as the outcome word).
    2. `?category=settings_change` returned the **failed sign-in**. A category with no outcome word
       produced *no* predicate, so the filter matched everything on that side; it now produces
       `false`, and `sign_in` was added to `outcome_filter` — it is this table's **default**
       category, so it must select its rows rather than refuse them (its first version returned
       nothing, and `?category=sign_in` answered zero while the unfiltered timeline showed one).
    3. `EVENT_COLUMNS` omitted `detail`, so the audit row's digest reached the screen and stopped
       at the export — the file attached to a ticket was thinner than the screen it came from.
       The containment assertion that was supposed to catch it could not; a row-width check can,
       and it is now in `events_csv.rs`.

    **What the screen does not claim.** Permission refusals are **absent**: the guard in
    `apps/api/src/guards.rs` answers `403` and records nothing, so `denial` resolves to the
    address rule's `blocked` outcome only. The screen states this in a permanent note rather than
    leaving an operator to conclude the platform records refusals it does not — recording one
    per refusal would put a database write on every refused request, and an attacker would decide
    how fast the audit table fills.

    **Two harness faults found while writing it, both worth naming.** The walk's header save was
    seeded with `directives: []` and then `name`/`sources`; the policy store correctly refused it
    twice (`a policy needs a "default-src" directive`, then `needs "script-src"`) because a CSP
    with no `script-src` is not a policy. The walk was creating a row the product rightly declined
    to write. And `Harness::fresh()` now sets `OMNION_IP_ACCESS_ALLOW_UNADDRESSED=1`: `oneshot`
    carries no `ConnectInfo`, and slice 4's own layer refuses an address-less request with
    `ip_unknown` while any rule is in force — a refusal that is **correct** and is asserted
    deliberately by `a_denied_network_cannot_reach_the_api`, but which turned every other walk in
    the file red once a rule existed. Set in the harness once, so the suite does not depend on an
    operator remembering an environment variable.

    **The screen box is still open**, for the same reason as every other one in this REQ: the
    browser pass has not run. `scripts/qa/walkthrough.cjs` visits `/security/events` in the
    desktop and the mobile pass (registered in both inventories) and asserts what a static
    inventory cannot: the honesty note is present, the counts line reads "N of M", and the
    category filter is **applied** rather than inert. `3f60ef77` registered the route and
    extended `runSecurityDepth`; every selector the walk asserts was checked to exist in the
    component (`counts`/`note`/`row`/`category`/`clear`), because a walk that selects a
    `data-` attribute the screen never renders is a walk that passes on an empty page — the
    same false green as an audit-only projection.

    **Recorded so it is not re-found: this REQ's ninth defect was the linker, not the code.**
    The first run of these tests died in `collect2` with `ld terminated with signal 7
    [Bus error]` while linking the `security` test binary. The diagnosis is `/mnt/apopic` at
    **100% (193 MiB free)**; the load average was 13–27 from sibling writers and 30 of 32 GiB
    of RAM was in use. Reclaiming **only this worktree's** `target/debug/incremental` (795 MiB)
    and then grouping `target/debug/deps` by `lib<crate>-<16 hex>` and deleting every copy but
    the newest (835 stale artifacts, 2 691 MiB) returned the disk to 95% / 3.4 GiB and the suite
    to green in one run. Three rules worth keeping: a linker bus error on this box is a disk
    symptom until proven otherwise; the reclaim must be scoped by `readlink /proc/<pid>/cwd`
    because a sibling writer had live cargo in `omnion-w6`; and `CARGO_INCREMENTAL=0` keeps the
    space from coming back as 115 retries.

    **Gates this tick:** `omnion-security` **188** · `omnion-api --lib` **281** · `--test
    security` **8/8** (42 s, live PostgreSQL) · `pnpm typecheck` **0** (`tsc --noEmit`, admin +
    web).
- [x] Secret inventory lists names and rotation age only; no value appears in HTML, JSON or export. — **closed 2026-10-02, `b877d429`…`d5c74b95`, and the release-blocker box is the one this REQ most nearly lost.**

  The requirement reads as a request for a secrets table. **There is no secrets table and there
  must not be one**: this is a projection over references, and its entire value is being the one
  screen in the security centre from which no secret can be read. Three constraints, and the
  point of each is that the guarantee is structural rather than a promise in a comment:

  * **`SecretRef` has no `value`, `ciphertext`, `hash` or `preview` field.** Adding one would make
    the struct able to carry a credential, and that ability *is* the risk. A unit test serialises
    a row and asserts no such key appears.
  * **There is no `healthy` state, and no `present` one either.** The platform can see that a
    reference exists and can read nothing about the value behind it, so a state meaning "this
    secret is fine" would have to be a lie. The strongest form of the rule is that the type
    cannot express it — and the first draft *did* carry a `Present` variant that no row produced,
    which would have let a future contributor render "present = fine". It was removed and the
    test rewritten to pin the whole vocabulary.
  * **The store selects explicit columns, never `*`.** `*` re-reads the source's own schema, so a
    source that gains a value column upstream would start appearing in a security screen with no
    code change here at all. The three sources holding real material (`webhook_endpoints.secret`,
    `service_account_keys.secret_hash`, `mfa_factors.secret_ciphertext`) are read as **counts**,
    and the count is what replaced the value.

  **The walk probes the VALUES, not the column names** — `apps/api/tests/security.rs::no_secret_value_reaches_the_inventory_response`. A name scan would pass an aliased column or a value inlined into a note; a literal scan only fails when the actual leak happens. It seeds a webhook secret, a service-account hash and a TOTP ciphertext into a live database, then asserts all three literals are absent from the response **while the reference name is present** — a reference is not a secret, and that asymmetry is the difference between a projection and a dump. The positives are asserted too: an empty body satisfies every containment check in the test while showing an operator a blank screen.

  **It found a real bug on its first run**: `column reference "expires_at" is ambiguous`, because
  `service_account_keys` and `service_accounts` both carry that column. The query read correctly in
  review and answered `500` on *every* inventory load — the tenth instance of this REQ's defect
  class, and the argument for why a walk drives the router rather than reading the SQL.

  **Rotation age is a reading, and the screen says which one.** Rotation happens outside the
  platform, so `rotated_at` is the reference row's own timestamp and `RotationEvidence` names
  whether it was **changed** or merely **created**; the panel renders `reference_created` as
  *"this is not a rotation date"* in words. An environment variable has no evidence at all and
  shows a dash rather than a blank cell that reads as "recently rotated".

  **The screen states its own blind spot in a permanent note**, because the environment list is
  maintained by hand — a process cannot enumerate its own environment, so this inventory cannot
  list a secret it was never told about. The API returns that as a `limitation` field and the
  panel shows it on every load; `the_limitation_is_not_optional` keeps it from being dropped.

  **Read-only, and proven so.** `the_inventory_cannot_be_written_through` drives all four write
  verbs with the **full** key set, so the refusal cannot be mistaken for a permission problem —
  it is the route shape refusing. A screen that could edit a *reference* would invite an operator
  to believe it could rotate a *secret*.

  **The walkthrough asserts the absence, which is the only way to test a deliberate omission**:
  it scans every button and link for `edit|rotate|revoke|delete|remove|update|replace|add` — a
  "coming soon" button would satisfy a presence check — and separately scans the rendered text
  for value-shaped tokens, because the browser is the last hop neither the type nor the API walk
  can see and a screenshot on a ticket is where a value would end up.

  208 crate tests, 18 new; the walk lives in `--test security`.
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
4. **IP access + events + inventory** — allow/deny evaluation, rules UI, security-event view, secret inventory projection, `security.finding.opened` webhook. **THREE OF FOUR PIECES CODE-COMPLETE.**

   **(a) IP access — 2026-10-02** (`0217_security_ip_rules.sql`, `crates/security/src/{ip_rules,ip_store}.rs`, `apps/api/src/security_ip.rs`, `apps/api/src/routes/security_ip.rs`, `features/security/ip-access.tsx`, `4ac78836`…`4b581fb8`): a denied CIDR is refused over the router with the rule named, the tester agrees with the layer on the same input, and CIDR validation is field-level for both families. Two decisions to keep: a request with **no** address is refused (`ip_unknown`) while rules are in force, because allowing it makes every in-process walk pass for the wrong reason; and a rule that blocks the caller is **warned about, not refused** — locking yourself out of one route while the panel is served from another is a legitimate move. Worth re-recording: the posture overview's IP-allow-list check linked to `/security/ip-access` from the moment the registry was written and that link was dead until this slice.

   **(b) Security-event view — 2026-10-02, backend + screen + walkthrough** (`264480a1`…`3f60ef77`): `crates/security/src/{events,events_store,events_csv}.rs`, `apps/api/src/routes/security_events.rs`, `GET /security/events` + `GET /security/events.csv`, `features/security/security-events.tsx`, `app/security/events/page.tsx`, `runSecurityDepth` extended. **The requirement's wording was half wrong and the fix is in the module doc**: "from the audit trail" names one of the two tables a security event lives in, and an audit-only projection answers `200` with an empty sign-in column on a platform meeting every requirement — which reads as a working filter. The timeline is a merge of `audit_log` and `sign_in_attempts` and every row names its `source`. 188 security + 281 api --lib + 8/8 `--test security`.

   **(c) Secret inventory — 2026-10-02, complete** (`b877d429`…`d5c74b95`): `crates/security/src/{secrets,secrets_store}.rs`, `GET /security/secrets`, `features/security/security-secrets.tsx`, `app/security/secrets/page.tsx`, the tab, the walkthrough entry. **The release-blocker box is closed** and it is the one this REQ most nearly lost — see its acceptance entry for why the containment is structural rather than a promise, and for the tenth instance of this REQ's defect class, which the walk found on its first run (`expires_at` ambiguous between two tables; the query read correctly and answered `500` on every load).

   **(d) The `security.finding.opened` webhook — 2026-10-03, complete** (`e15d1880`, `25306c7b`, `d6c26218`, `73e890b6`). The event name joins the catalogue with **identity fields only** — `finding_id`, `severity`, `source` and the package triple. `title`, `description` and `evidence` are absent, and that is the design rather than an omission: all three are content that came from *outside* (a CI vendor's package name, its prose, the raw entry), and this is the first security payload that fans out to a receiver outside the operator's own infrastructure. The walk seeds a subscribed endpoint, ingests a report with a recognisable literal in **every** content field — including the operator `note`, which no receiver needs — then reads the **queued** payload back and asserts each literal is absent while the triage fields are present and `finding_id` resolves to the real row. Values, not column names: a scan for `"title"` would pass a payload that nested or inlined it.

   **The walk also proved the emitter does not fire on a finding that was already known**, which containment alone cannot supply — a nightly CI job re-ingests every morning, so an emitter on the upsert regardless of its branch is an event an operator learns to ignore.

   **And it found the eleventh instance of this REQ's defect class on its first run, and this one was the store's.** `upsert_finding` asked `coalesce(xmax, 0)` — `xmax` is an `xid`, so Postgres raises `COALESCE types xid and integer cannot be matched` on *every* version. The answer sat in a second `select` with `.unwrap_or(true)`, so the error was swallowed and **every ingest since the function was written reported every finding as newly created**. The panel's re-ingest protection has been showing `created: N` instead of `created: 0, refreshed: N`, and the acceptance criterion below was ticked on a return value that had never answered the question. `xmax = 0` now rides the same statement's `RETURNING`, so one round trip answers created-or-refreshed and a failure surfaces as the store error it is (`d6c26218`).

   208 security + 49 events + 284 api --lib + **11/11** `--test security` + the drift gate green.

   Still open, and it is a screen box for (a) and (b) alike: the browser pass. **The box was saturated for this whole tick** — load average 17.8, 30 of 32 GiB RAM in use, 55 Chrome processes, and the QA slot genuinely held live by `w3` (`pid 2624054`, `cwd=/mnt/apopic/omnion-w3`, verified with `kill -0` **and** `/proc/<pid>/cwd`, not by the age of the placeholder). Two consecutive ticks have now recorded a deferral for the same reason, which makes it a standing risk rather than bad luck: **five** screen boxes across this REQ turn on a pass that has not run. The next tick that finds a free slot runs it and closes or names them.

### Risks / notes

- Never store or echo a secret value; the inventory is a projection over references. A leaked value in logs, audit payloads or CSV export is a release blocker.
- Rate limiting must fail open on a Redis outage but log it — a limiter that takes the platform down is worse than no limiter; the setting exposes this choice explicitly.
- Lockout can be weaponised against a known account: count per IP as well as per account, and keep the unlock path (including a CLI path) working when the panel is unreachable.
- Config-based checks are only as truthful as their inputs: a check that cannot verify something must report `unknown` with the reason, never `pass`.
- CSRF tokens must not break the public renderer or webhook intake; those routes are exempt by design and the exemption list lives in one place.
