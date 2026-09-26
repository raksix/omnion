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

- [x] First-run wizard (admin) + `omnion` CLI skeleton (`setup`, `doctor`, `migrate`) per docs/04 tools/cli.
      `crates/onboarding` (the flow both front ends drive: owner → organization → site(+domain) → theme → AI step → done, with the derived step status and the getting-started checklist), `tools/cli` (the `omnion` binary), the `/api/v1/onboarding` surface in `apps/api`, the `/setup` wizard + dashboard checklist in `apps/admin`, and migration `0007` (`sites.theme` + the `onboarding_state` singleton).
- [x] Flow: owner account → organization → first site → theme → (optional) AI provider → done checklist.
      The wizard resumes from the server-derived steps (a refresh picks up at the first open one); the owner step signs the account in and binds the Owner role; the dashboard keeps the checklist, which ticks itself off as pages are published and a domain is bound. The AI step records the skip — provider connections arrive with the AI Hub (P11).
- [x] **Verify:** fresh-DB E2E: wizard → working admin, no manual SQL. Commit + push + log.
      `apps/api/tests/onboarding.rs` (4 walks on throwaway databases), the browser walk (10 checks, 0 console errors), the CLI walk (`omnion doctor`/`migrate`/`setup` on a fresh database) and the CI smoke extension.

## P11 — AI Hub v0 (docs/06)

- [x] Provider abstraction + providers/models tables; chat endpoint (streaming); provider config in admin.
      `crates/ai-hub` (stored shapes with validation, the OpenAI-compatible wire client with an SSE
      decoder, and the router that resolves `provider/model` → bare key → default model),
      `database/migrations/0008_ai_hub.sql` (two partial unique indexes: one default provider, one
      default model), the `/api/v1/ai` surface (providers — the key write-only —, the model registry,
      discovery against the provider itself, `PATCH /ai/models/{id}` and `POST /ai/chat` answering as
      `text/event-stream`), the `/ai` screen in `apps/admin` (providers, models, Try-it chat) and
      `infra/mocks/openai-compatible.mjs` for the mock round trip.
- [x] **Verify:** chat round-trip against a configured OpenAI-compatible provider (CI: mock). Commit + push + log.
      `apps/api/tests/ai_hub.rs` — 2 walks against the compose stack with an in-process mock provider
      (connect → registry → streamed chat → refusal → default repair → audit → removal, plus the
      permission gate). Live walk on `:18096` against the Node mock: `start` + 5 deltas + `done`
      (`total_tokens=12`) reassembling to the mock's answer, a refused model as an `error` frame with
      `provider_error`, anonymous `401`, 7 `ai.*` audit rows. Browser walk 10/10 with 0 console errors.
      The CI `AI Hub walk (mock provider)` step runs the same flow (dry-run locally: exit 0).

## P12 — Events + Webhooks v0

- [x] Event bus table + delivery worker + HMAC signatures; first fan-out: `page.published`.
      `crates/events` (the bus, the endpoint + queue shapes, the HMAC-SHA256 scheme, the signed
      sender and the delivery runner), `database/migrations/0009_events_webhooks.sql` (`events`,
      `webhook_endpoints`, `webhook_deliveries`), the `/api/v1/webhooks` + `/api/v1/events` surface
      (endpoints CRUD, the operator's test delivery, the queue history, the event feed; the signing
      secret is write-only), `apps/api/src/event_runner.rs` driving the queue in the API process
      (`OMNION_EVENTS_*`), the `webhooks.*`/`events.read` permissions, and `infra/mocks/
      webhook-receiver.mjs` + `infra/mocks/webhooks-walk.sh` — the receiver both a developer and CI
      verify a signed delivery with.
- [x] **Verify:** local receiver test shows signed delivery. Commit + push + log.
      `apps/api/tests/events.rs` (2 walks, real loopback receiver: fan-out, retry ladder, terminal
      failure, tenant scope, switched-off endpoint, permission gates) plus the live walk on
      `:8082`/`:8124` — `receiver: event=page.published signature=verified` ·
      `platform: status=delivered attempts=1 response=200` — and the `Webhooks walk (signed
      delivery)` CI step running the same script.

## P13 — Automation v0 (REQ-003 lite)

- [x] Trigger → condition → action on top of P09; first actions: send email (mailpit in dev), create content revision comment.
      `crates/automation` (a rule IS a workflow with `trigger_kind = 'event'`; the closed 9-operator
      condition set; `{{ }}` bindings; the one-transaction drain whose cursor row is the lock),
      `send_email` (SMTP written out in `mail.rs`; `OMNION_MAIL_*`) + `comment_revision`
      (`crates/content/src/comments.rs`), `database/migrations/0010_automation.sql` (cursor + runs +
      the P09 `'event'` constraint work), `/api/v1/automations` CRUD + `/automations/catalogue`,
      `apps/api/src/automation_runner.rs` (`OMNION_AUTOMATION_*`).
- [x] **Verify:** automation runs E2E on a page.published trigger. Commit + push + log.
      `apps/api/tests/automation.rs` — 5 walks on a throwaway DB with a real in-process SMTP sink:
      the full page.published → conditions → send_email + comment_revision run end to end (the sink
      reads the composed subject/recipient/body; the revision comment is readable; the audit rows are
      there), a false condition and an unheard event start nothing, an unfillable binding refuses the
      run, a switched-off mail server fails the step with that reason, and the surface is
      permission-gated + tenant-scoped. Full suite: `cargo test --workspace --lib` 328 passed ·
      `cargo test -p omnion-api --tests` 58 + 56. fmt + clippy clean.

## P14 — Polish + CI v0

- [x] CI workflow runs for real (fmt/clippy/test + admin build); README quickstart (`docker compose up -d`).
      The workflow gains the `Web — install · typecheck · build · serve (admin + public renderer)` job:
      pnpm comes from the root `packageManager` field (`pnpm/action-setup@v4`), the lockfile is installed
      frozen, `pnpm typecheck` + `pnpm build` cover both Next.js apps through turbo, and the job boots the
      panel and checks that the sign-in screen answers `200` while an anonymous panel route is redirected
      (`307`) to it — no API needed, exactly what a fresh clone can show. The README is rewritten around a
      verified quickstart (compose stack → API → first run → panel → public renderer) plus the checks, the
      repository layout and the documentation map.
- [x] **Verify:** CI green on push. Commit + push + log.
      Run `36213754270` (head `3a8de29`) → **success**, all three jobs: `Web …` (40 s) logged
      `admin panel: /login=200 · anonymous /pages=307 → /login`; `Rust — fmt · clippy · test` (with the
      smoke, AI-Hub, webhooks and first-run walks) and `Infra — compose config` stayed green. Local dry
      runs of the same steps: `pnpm install --frozen-lockfile` → "Already up to date" · `pnpm typecheck`
      → 2/2 · `pnpm build` → 2 successful · panel `/login=200` / `/pages=307` · API `/healthz` 200 ·
      `/readyz` database + redis ok · `omnion doctor` 6/6.

## P15+ — REQ-driven queue

After P14 the loop works `docs/requests/REQ-XXX` (ascending by default; the owner/chat curates
priority). Suggested early order: REQ-016 (webhooks) → REQ-002 (command center) → REQ-021
(notifications) → REQ-012/013/014 (admin centers) → REQ-050 extensions.

## P-DEP — Dev deployment (GATED — "önce kod, sonra yayın")

Runs ONLY on explicit owner go. Target: **omnion.fermag.com.tr** (dev).
Checklist when triggered: DNS check → nginx vhost + TLS → compose stack (postgres/redis/minio)
→ pm2/systemd services for `omnion-api` (+ admin/web when ready) → smoke test (`/healthz`,
`/readyz`, login). Live-verify with curl and log evidence.
