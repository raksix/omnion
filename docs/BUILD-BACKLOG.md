# Omnion — Build Backlog

> Execution queue for the **`omnion-build`** loop. **ONE phase-task per run.** Cross-tick
> memory: [`docs/BUILD-LOG.md`](BUILD-LOG.md). Working rules: `docs/00-CONTEXT.md`
> (public-repo wording, English atomic commits + immediate push, keep the Core thin).

## How this queue works

- Phases run in order **P00 → …**. Each phase has explicit tasks; the loop picks the FIRST
  unchecked task, finishes it completely, verifies it with real commands, commits + pushes,
  and appends a log row.
- **Gated phases** (P-DEP) run only when the owner explicitly says so.
- After P14 the loop consumes `docs/requests/REQ-XXX` files in priority order (owner steers;
  default = ascending, skipping anything already delivered).

## P00 — Toolchain + repo skeleton

- [x] Install Rust toolchain (rustup, minimal profile) → `cargo --version`. `cc`/`gcc` 13 + `ld` already present.
- [x] Root `Cargo.toml` workspace (`resolver = "2"`, members: `apps/api`, `crates/*`) + `rust-toolchain.toml` (stable).
- [x] `apps/api`: axum app, `GET /healthz` → `{"ok":true}`, `PORT` env (default 8080).
- [x] `crates/core`: `omnion-core` lib crate (documented empty base for later phases).
- [x] `infra/compose/docker-compose.dev.yml` + `postgres.yml` + `redis.yml` + `minio.yml` + `mailpit.yml` (per docs/04 + docs/02).
- [x] `.gitignore` additions (`target/`, `.env`), `.github/workflows/ci.yml` skeleton (fmt + clippy + test).
- [x] **Verify:** `cargo check` green; `curl localhost:8080/healthz` → 200; `docker compose config` valid. Commit + push + log.

## P01 — Core foundations

- [x] Typed env config module + `tracing` subscriber (OpenTelemetry-ready hooks) + shared error type.
- [x] sqlx + Postgres pool + migration runner; `database/migrations/0001_initial.sql` (organizations, users, sessions, audit_log minimal per docs/07).
- [x] `GET /readyz` → DB + Redis pings.
- [x] **Verify:** `docker compose up -d` + API against it; migrations applied; readyz shows both ok. Commit + push + log.

## P02 — Identity v0 (docs/07 subset)

- [x] Users (+ argon2 hashing), sessions, `POST /api/v1/auth/login`, `GET /api/v1/me`, `POST /api/v1/auth/logout`.
- [x] First-admin bootstrap on empty DB (env `OMNION_ADMIN_EMAIL` / `OMNION_ADMIN_PASSWORD`, hashed at boot).
- [x] Integration tests against compose DB.
- [x] **Verify:** curl login → cookie → me flow; tests green. Commit + push + log.

## P03 — IAM v0 (docs/07)

- [x] Tables: roles, permissions, role_bindings (scope columns: organization/site), role inheritance flag.
- [x] Guard middleware `require(permission)` + audit write on privileged actions.
- [x] Seed base roles from docs/07 §3 (Owner 1000 → Member 100) + permission catalogue for existing endpoints.
- [x] **Verify:** permission-gated endpoint tests (allowed vs denied vs explicit-deny). Commit + push + log.

## P04 — Tenancy v0

- [x] organizations, sites, domains tables + CRUD API; scope enforcement (user sees only their org/site scope).
- [x] **Verify:** tests incl. cross-tenant denial. Commit + push + log.

## P05 — Content v0 (docs/05 §4 + docs/01 §5)

- [x] pages + revisions (immutable rows, `revision_no`), draft/published states, publish/restore endpoints, slug rules.
- [x] translations table skeleton (content → translations[lang] model, NOT `title_tr` columns).
- [x] **Verify:** tests incl. restore-to-previous-revision. Commit + push + log.

## P06 — Admin app v0

- [x] `apps/admin`: Next.js + TS + Tailwind; login page (API-wired), app shell, pages list, site switcher (read-only ok).
- [x] Root `package.json` + `pnpm-workspace.yaml` + `turbo.json` skeleton (docs/04).
- [x] **Verify:** production build green; dev server serves; login works against local API. Commit + push + log.

## P07 — Public web v0

- [x] `apps/web`: minimal server-side renderer for published pages; theme engine stub with `themes/minimal` (docs/03); `GET /:slug` renders.
      Public surface `GET /api/v1/public/pages/{slug}` (published revisions only; site resolved from `?site=` → `Host` → the only site), `packages/types` + `packages/theme-sdk` (theme contract) + `themes/minimal` (manifest, layout, stylesheet).
- [x] **Verify:** curl a published page → HTML contains title + body. Commit + push + log.

## P08 — Media v0

- [x] `crates/storage` (S3/MinIO abstraction) + upload endpoint + media table + public serve path (dev: MinIO).
- [x] **Verify:** upload → fetch round-trip; file listed in admin. Commit + push + log.

## P09 — Workflow engine v0 (docs/09 lessons)

- [x] Durable step store (Postgres), background runner in `apps/api`, step retries (cap 5, backoff), wait-sweeper (batch, 30s), execution + step tables. `crates/workflows` + `0006_workflows.sql` + `apps/api/src/workflow_runner.rs`; the sweeper also reclaims claims whose runner stopped (lease) and settles runs left open.
- [x] Triggers v0: manual + schedule; cancellation flag. `POST /workflows/{id}/run` (manual), a five-field cron schedule in UTC armed at creation/update (`next_run_at`), `POST /workflow-executions/{id}/cancel` closing the run and its open steps.
- [x] **Verify:** 3-step workflow with one transient failure → retried; a wait step resumes; audit rows written. Commit + push + log. Live walk on `:18091` (retry `attempt=1 of=3`, wait parked+resumed, `attempts=2`, audit `started`/`completed`/`cancelled`), integration suite `apps/api/tests/workflows.rs` 11/11, workspace 250 tests green, CI smoke extended.

## P10 — Onboarding v0 (REQ-050)

- [ ] First-run wizard (admin) + `omnion` CLI skeleton (`setup`, `doctor`, `migrate`) per docs/04 tools/cli.
- [ ] Flow: owner account → organization → first site → theme → (optional) AI provider → done checklist.
- [ ] **Verify:** fresh-DB E2E: wizard → working admin, no manual SQL. Commit + push + log.

## P11 — AI Hub v0 (docs/06)

- [ ] Provider abstraction + providers/models tables; chat endpoint (streaming); provider config in admin.
- [ ] **Verify:** chat round-trip against a configured OpenAI-compatible provider (CI: mock). Commit + push + log.

## P12 — Events + Webhooks v0

- [ ] Event bus table + delivery worker + HMAC signatures; first fan-out: `page.published`.
- [ ] **Verify:** local receiver test shows signed delivery. Commit + push + log.

## P13 — Automation v0 (REQ-003 lite)

- [ ] Trigger → condition → action on top of P09; first actions: send email (mailpit in dev), create content revision comment.
- [ ] **Verify:** automation runs E2E on a page.published trigger. Commit + push + log.

## P14 — Polish + CI v0

- [ ] CI workflow runs for real (fmt/clippy/test + admin build); README quickstart (`docker compose up -d`).
- [ ] **Verify:** CI green on push. Commit + push + log.

## P15+ — REQ-driven queue

After P14 the loop works `docs/requests/REQ-XXX` (ascending by default; the owner/chat curates
priority). Suggested early order: REQ-016 (webhooks) → REQ-002 (command center) → REQ-021
(notifications) → REQ-012/013/014 (admin centers) → REQ-050 extensions.

## P-DEP — Dev deployment (GATED — "önce kod, sonra yayın")

Runs ONLY on explicit owner go. Target: **omnion.fermag.com.tr** (dev).
Checklist when triggered: DNS check → nginx vhost + TLS → compose stack (postgres/redis/minio)
→ pm2/systemd services for `omnion-api` (+ admin/web when ready) → smoke test (`/healthz`,
`/readyz`, login). Live-verify with curl and log evidence.
