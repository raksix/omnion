# Omnion — Build Log

> Cross-tick memory for the **`omnion-build`** loop. Newest entries at the bottom. 3-5 lines
> per entry: phase · what got done · proof (command + result) · next.

## 2026-09-25 — Loop bootstrap (chat session)

- Build phase opened by owner directive: Lokma-style loop, start building; code first, deploy later (dev target: omnion.fermag.com.tr).
- Created: `docs/BUILD-BACKLOG.md` (P00→P14 + gated P-DEP) + this log + `omnion-build` cron loop (pinned model).
- Toolchain status: node 22 ✓ · pnpm 11 ✓ · bun ✓ · docker 29 ✓ · gcc 13 ✓ · **Rust NOT installed → P00 installs rustup**.
- Next: **P00 — Toolchain + repo skeleton.**

## 2026-09-25 — P00 · Toolchain + repo skeleton

- Rust toolchain installed via rustup (minimal, stable): `cargo 1.98.1` · `rustc 1.98.1`.
- Workspace: root `Cargo.toml` (resolver 2, members `apps/api` + `crates/*`) + `rust-toolchain.toml` (stable, rustfmt + clippy).
- `apps/api` (thin HTTP layer, docs/04): `src/lib.rs`, `main.rs`, `state.rs`, `routes/{mod,health}.rs` —
  `GET /healthz` → `{"ok":true,"service":"omnion-api","version":"0.1.0"}`, `PORT` env (default 8080, invalid value fails fast);
  `crates/core`: `omnion-core` infrastructure lib exposing `BuildInfo`, consumed by the API state.
- Infra: `infra/compose/docker-compose.dev.yml` (+ `postgres.yml`, `redis.yml`, `minio.yml`, `mailpit.yml`) with overridable
  host ports; `.gitignore` gains `target/`; CI skeleton `.github/workflows/ci.yml` (fmt · clippy `-D warnings` · test + compose config check).
- Proof: `cargo check --workspace --all-targets` → Finished ✅ · `cargo clippy --workspace --all-targets -- -D warnings` → clean ✅ ·
  `cargo test --workspace` → 2 passed (router `/healthz` integration test + core metadata) ✅ ·
  `curl -i 127.0.0.1:8080/healthz` → `HTTP/1.1 200 OK` body `{"ok":true,"service":"omnion-api","version":"0.1.0"}` ✅ ·
  `PORT=18080` → 200 ✅ · `docker compose -f infra/compose/docker-compose.dev.yml config --quiet` → valid, services
  `postgres, redis, minio, mailpit` ✅.
- CI on GitHub: workflow run `36193827205` → **success** (jobs: `Rust — fmt · clippy · test` ✅ · `Infra — compose config` ✅).
- Repo note: this clone's `origin` moved to the SSH URL — the stored OAuth token has no `workflow` scope, so over HTTPS GitHub rejects any commit touching `.github/workflows/`.
- Next: **P01 — Core foundations** (typed env config + tracing subscriber + shared error type, sqlx/Postgres pool + `database/migrations/0001_initial.sql`, `GET /readyz` with DB + Redis pings).

## 2026-09-25 — P01 · Core foundations

- `crates/core` gained the shared infrastructure layer: typed `config` (OMNION_* keys, `PORT` fallback,
  validation for environment/port/pool size/URL schemes, pretty-vs-JSON log defaults), `telemetry`
  (tracing subscriber with the OpenTelemetry layer hook plus a shutdown handle), `error`
  (`ConfigError` + `CoreError`), `db` (SQLx pool + embedded migration runner) and `redis_client`
  (lazily connected handle that reconnects without a restart).
- `database/migrations/0001_initial.sql`: organizations, users (case-insensitive unique email),
  sessions (hashed tokens only) and an append-only `audit_log` whose `actor_type` also covers the
  AI Hub audit chain.
- `apps/api` boots config → telemetry → database + migrations → redis → HTTP, with SIGTERM/Ctrl-C
  graceful shutdown. `GET /readyz` pings both dependencies — `200` only when everything answers,
  `503` with detail in development — and `ApiError` maps core errors onto the HTTP surface (`503`
  for an unavailable dependency, `500` otherwise).
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **27 passed** (20 core unit · 3 api unit · 1 healthz ·
  3 readyz integration against the live compose stack) · live run: `GET /healthz` → `200
  {"ok":true,…}` and `GET /readyz` → `200 {"checks":{"database":{"status":"ok"},"redis":{"status":"ok"}}}` ·
  `_sqlx_migrations` shows version 1 "initial" with `success = t`, and `\dt` lists
  organizations · users · sessions · audit_log · failure path: with Redis stopped, `/readyz` → `503
  redis unavailable (broken pipe)` while `/healthz` stayed `200`, and it returned to `200` without
  restarting the API once Redis was back.
- Dev stack: `docker compose -f infra/compose/docker-compose.dev.yml up -d` now really starts all four
  containers (postgres 5433 · redis 6380 · minio 9000/9001 · mailpit 1025/8025). Upstream MinIO images
  are no longer published on Docker Hub (`pull access denied`), so `infra/compose/minio.yml` pulls the
  community mirror `pgsty/minio` — the same server binary.
- CI: the Rust job now provisions PostgreSQL + Redis service containers (so the readiness integration
  tests run for real) and adds a smoke step that boots the API and curls `/healthz` + `/readyz`.
- CI run `36195108450` → **success** (Rust job `1m51s`, Infra job `5s`): the readyz suite passed 3/3
  against the service containers and the smoke step logged `listening address=0.0.0.0:8080`,
  `{"ok":true,…}` for `/healthz`, `{"checks":{"database":{"status":"ok"},"redis":{"status":"ok"}}}` for
  `/readyz`, then `shutdown signal received` after SIGTERM.
- Next: **P02 — Identity v0** (users + argon2, sessions, login/logout, first-admin bootstrap,
  integration tests).

## 2026-09-25 — P02 · Identity v0

- New crate `crates/identity` (docs/07-IAM.md subset): `users` (create/find by email or id,
  lowercase-normalized addresses, case-insensitive uniqueness), `password` (Argon2id hashing on the
  blocking pool, minimum length policy, `dummy_verify` so unknown addresses burn the same work as
  wrong passwords), `sessions` (256-bit hex tokens; only the SHA-256 hash is stored; 30-day TTL;
  resolve/touch/revoke) and `authentication` (`authenticate` returns Authenticated /
  InvalidCredentials / AccountDisabled — account status is only disclosed after the password
  verified).
- `crates/core` config gains `OMNION_ADMIN_EMAIL` + `OMNION_ADMIN_PASSWORD` (both-or-neither
  validation, password redacted from `Debug`); `apps/api` boot now seeds the first administrator on
  an empty database (`bootstrap_first_admin`, idempotent and race-safe) and logs why it skipped
  otherwise.
- API surface: `POST /api/v1/auth/login` (session cookie `omnion_session`, HttpOnly + SameSite=Lax,
  `Secure` outside development, Max-Age 30 days) · `GET /api/v1/me` — backed by a `CurrentSession`
  extractor (401 `unauthenticated` / `invalid_session`) · `POST /api/v1/auth/logout` (204, revokes and
  clears). Identity errors map onto the HTTP surface through `ApiError` (exhausted pool → 503).
  `ClientAddress` extractor captures the peer IP for `sessions.ip_address` without requiring
  connect-info (axum has no optional `ConnectInfo`), and the server is served with
  `into_make_service_with_connect_info`.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **65 passed** (identity 18 · core 23 · api unit 14 · health 1 ·
  readyz 3 · auth integration 6 — including bootstrap on a throwaway database and revoke-after-logout)
  · live run on `:18080` against the compose stack: boot log `first administrator account created`,
  `/healthz` 200, `/readyz` 200 both checks ok, login 200 + `set-cookie: omnion_session=…; Path=/;
  HttpOnly; SameSite=Lax; Max-Age=2592000` with a 64-char token, `/me` 200 with the account JSON,
  `/me` without cookie 401 `unauthenticated`, logout 204 + `Max-Age=0`, replayed revoked token 401
  `invalid_session`, wrong password 401 `invalid_credentials`; database check: `admin@omnion.test`
  row with `$argon2id$v=19$…`, one session row with a hash that is not the token, `revoked = t`,
  `seen = t`.
- CI: the smoke step now seeds the admin and walks login → me → anonymous 401 → logout.
- CI run `36196441876` → **success**: `Rust — fmt · clippy · test` ✅ (1m11s — the identity suite ran
  against the service containers) · `Infra — compose config` ✅ (4s).
- Next: **P03 — IAM v0** (roles, permissions, role bindings, `require(permission)` guard, audit
  writes, role seeds from docs/07 §3).

## 2026-09-25 — P03 · IAM v0

- New crate `crates/permissions` (docs/07-IAM.md): the permission catalogue (32 keys across
  content/media/users/plugins/deployment/iam/audit), fully custom roles with priority, an inheritance
  link and flag, explicit allow/deny entries and scoped role bindings (global / organization / site,
  `expires_at` for temporary roles). `evaluate::RoleGraph` is a pure, deterministic resolver for the
  documented precedence — explicit deny > explicit allow > inherited allow > default deny — and it
  reports the provenance of every verdict (the seed of the permission simulator, §18). `crates/audit`
  is the append-only trail writer (§13, §19) reused by later AI-agent actions.
- Migration `0002_iam.sql`: `permissions`, `roles` (platform vs organization roles, key unique per
  scope), `role_permissions` (allow/deny, one row per key) and `role_bindings` (scope shape enforced by
  a check constraint, live combinations unique, `site_id` gains its foreign key with P04).
- `apps/api`: `require(permission)` guard (`src/guards.rs`) wraps routes, resolves the session once and
  hands it to the handler through the request extensions; privileged actions write audit rows in the
  same request. New `/api/v1/iam` surface: permissions, roles (GET/POST), role permissions (PUT),
  bindings (GET/POST), effective-permissions (own set without a permission, others need
  `iam.roles.read`) and audit (GET, `audit.read`). Boot seeds the catalogue and the six base roles
  (§3) and keeps the "at least one Owner" invariant (§20), auditing the fallback binding as a platform
  action.
- Fixed: adding a migration did not rebuild `omnion-core`, so the embedded migrator kept the old set
  and `0002` silently never applied (`relation "permissions" does not exist`). `crates/core/build.rs`
  now emits `cargo:rerun-if-changed=../../database/migrations`.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **95 passed** (permissions 20 unit incl. deny/inheritance/cycle
  cases · audit 2 · api unit 17 · identity 18 · iam integration 4 against the compose stack).
- Live run on `:18081` against the compose stack: boot log `permission catalogue and base roles ready
  permissions=32`; `/healthz` 200 and `/readyz` both checks ok; anonymous `/api/v1/me` 401 and
  `/api/v1/iam/roles` 401; a signed-in session without a role gets `403 permission_denied` naming
  `iam.roles.read`; after the platform Owner binding `GET /iam/roles` → 200 with the ladder (`owner
  p1000 allow=32`, `administrator p900 allow=32`, `manager p700 allow=21`, `moderator p500 allow=9`);
  `POST /iam/roles` 201, `PUT /iam/roles/{id}/permissions` 200, `POST /iam/bindings` 201, editing a
  platform role 403 `system_role`; `/api/v1/iam/audit` lists `iam.binding.granted`,
  `iam.role.permissions_updated` and `iam.role.created` with actor, target and metadata, and the same
  rows are in `audit_log`.
- CI: the smoke step now also walks the gated surface (roles + own effective set, anonymous 401 for
  roles/audit). Run `36197985861` → **success**: `Rust — fmt · clippy · test` ✅ (1m06s; the smoke log
  shows `permission catalogue and base roles ready permissions=32`, the six base roles with the
  documented priorities and allow counts 32/32/21/9/8/2, the administrator's global Owner binding and
  the resolved effective set) · `Infra — compose config` ✅.
- Next: **P04 — Tenancy v0** (organizations, sites, domains + CRUD API, scope enforcement,
  cross-tenant denial tests).

## 2026-09-25 — P04 · Tenancy v0

- Migration `0003_tenancy.sql` (docs/01-VISION.md §10, docs/07-IAM.md §7): `sites` (one property
  of one organization, `key` unique per organization) and `site_domains` (host unique
  platform-wide, at most one primary per site through a partial unique index). The site-scoped
  role binding finally gets its foreign key; the narrow pre-constraint cleanup retires bindings
  that pointed at no site, so the constraint applies on an existing database.
- `crates/identity` grows the tenancy store: `organizations` (create/read/list/update/delete with
  slug, name and status validation) and `sites` (site and domain CRUD, host validation, automatic
  first-primary plus promotion on removal, `find_site_by_host` — the routing primitive P07 builds
  on). `IdentityError` learns the tenancy variants; `apps/api/src/error.rs` maps them
  (404 not-found, 409 taken, 400 shape).
- `crates/permissions`: new `tenancy` category — `organizations.read|manage`,
  `sites.read|create|update|delete`, `domains.manage`; the Manager ladder gains site management
  and Moderator/Editor `sites.read`; `bindings::validate` now refuses a site scope that names an
  unknown site or one of another organization.
- `apps/api`: `/api/v1/organizations` (GET/POST/GET id/PATCH/DELETE) and `/api/v1/sites`
  (GET/POST/GET id/PATCH/DELETE) plus `/{id}/domains`, `/{id}/domains/{domain_id}` and
  `/{id}/domains/{domain_id}/primary`, all behind the new guards. The shared `scope.rs` helpers
  carry the tenancy rule — an account with a primary organization stays inside it, opening and
  deleting a tenant is platform-only — and `routes/iam.rs` now uses them instead of its own
  copies. Deleting a tenant requires it to be empty (`409 organization_not_empty`).
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **119 passed** (identity 27 · permissions 22 · core 23 · api
  unit 26 · auth 6 · health 1 · iam 4 · readyz 3 · tenancy 5 integration against the compose
  stack, incl. cross-tenant denial and restore-free domain promotion).
- Live run on `:18082` against the compose stack (Owner session, P03 pattern): `/healthz` and
  `/readyz` both ok; anonymous `/api/v1/sites` and `/api/v1/organizations` 401; tenant 201,
  duplicate slug 409 `organization_slug_taken`; site 201, duplicate key 409 `site_key_taken`; first
  host primary, second non-primary, promotion 200, list primary-first; platform patch 200; domain
  removed 204 with the remaining host promoted; site 204; tenant 204 and `GET` 404; `site.created`,
  `site.domain.added`, `site.domain.primary_changed`, `site.updated`, `site.domain.removed`,
  `site.deleted`, `organization.created`, `organization.updated` and `organization.deleted` rows
  in `audit_log` with actor, target and metadata.
- Cross-tenant, live: an organization account (its own role with the tenancy keys, bound at
  organization scope) sees exactly its own organization and site and may create inside them, while
  every read of the other tenant — organization, site, domains, `?organization_id=` filter — and
  every write (`POST /sites` with the other organization, `POST /sites/{other}/domains`, PATCH,
  DELETE) answers **403 cross_organization**; the same calls on its own site answer 200/201. An
  account without a role gets **403 permission_denied** on the way to the handler; opening a
  tenant, and deleting its own, answer **403 platform_only** while renaming it is 200; a
  site-scoped binding that names another tenant's site is refused with `invalid_request`
  (“the site belongs to another organization”). The dev database was left clean
  (organizations 0 · sites 0 · domains 0 · 6 base roles · 1 owner binding).
- Noted for operators: base roles keep the permission set they were created with, so an
  installation that upgrades keeps the new tenancy keys on the Owner only — a tenant grants them
  to a role of its own (proven live); the seed's Owner sync is what keeps the platform owner
  complete.
- CI: the smoke step now also opens a tenant, a site and a domain and re-checks the anonymous 401s;
  the block was rehearsed locally against the built binary before pushing.
- CI run `36199468683` → **success**: `Rust — fmt · clippy · test` ✅ (119 tests, including the five
  tenancy integration tests; the smoke log shows tenant `smoke-organization`, site `main` and domain
  `smoke.omnion.test` with `"is_primary":true` created against the CI service containers) ·
  `Infra — compose config` ✅.
- Next: **P05 — Content v0** (pages + revisions, draft/published, publish/restore, slug rules,
  translations skeleton).

## 2026-09-25 — P05 · Content v0

- New crate `crates/content` (docs/05-VERSIONING.md §4–§7, docs/01-VISION.md §5, §7): `pages`
  (slug unique per site, `page_type`, lifecycle `draft`/`published`/`archived`, a pointer at the
  revision visitors see), `page_revisions` (append-only history with `revision_no`,
  title/body/summary and `restored_from_id`) and `translations` (one value of one field of one
  resource in one language — the content → translations[lang] model, no `title_tr` columns
  anywhere).
- The store keeps the documented rules: revision 1 is written with the page; editing content
  appends `n + 1` and archives the draft it supersedes; publishing freezes the draft, retires
  the revision it replaced and refuses with `no_draft_revision` when nothing is pending;
  restoring copies an older revision forward as a new draft (recording where it came from), so
  history is never rewritten; deleting a page takes its revisions and translation rows with it.
  Migration `0004_content.sql`; partial unique indexes hold "at most one draft, one published"
  in the database itself.
- API surface: `GET|POST /api/v1/pages`, `GET|PATCH|DELETE /api/v1/pages/{id}`,
  `POST /{id}/publish`, `POST /{id}/restore`, `GET /{id}/revisions[/{revision_id}]` and
  `GET|PUT /{id}/revisions/{revision_id}/translations[/{language}]`, all behind the
  `content.pages.*` guards plus the tenancy scope rule (a foreign page answers `403
  cross_organization`); every state change writes an audit row (`page.created`, `page.updated`,
  `page.published`, `page.revision.restored`, `page.translation.updated`, `page.deleted`).
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D
  warnings` clean · `cargo test --workspace` → **140 passed** (content crate 12 unit · api
  content routes 3 unit · 5 content integration against the compose stack). Live run on `:18083`
  against a fresh database (dropped afterwards): page `home` created as v1 draft; publish →
  `published_rev=1`; edit → v2 draft while v1 stayed live; publish → v2; re-publish →
  `no_draft_revision`; restore v1 → v3 draft with `restored_from=<v1>`; publish → v3 live;
  history `v3 published / v2 archived / v1 archived` with v1's row unchanged;
  `PUT .../translations/tr` → `tr/title = Merhaba`, `tr/body = Gövde`; anonymous `/api/v1/pages`
  → `401 unauthenticated`; `_sqlx_migrations` shows version 4 `content`; `audit_log` carries the
  seven `page.*` actions with revision numbers.
- CI: the smoke step now walks the content surface (create → publish → edit → publish → restore
  → translation) and re-checks the anonymous 401. Run `36200626747` → **success** (Rust job
  1m31s; the smoke log shows the page created as a draft, revised to "Welcome to Omnion", and
  the restore row `revision_no: 3, state: draft, restored_from_id: …`) · `Infra — compose
  config` ✅.
- Next: **P06 — Admin app v0** (`apps/admin` Next.js + TS + Tailwind with the API-wired login,
  app shell, pages list and site switcher; root `package.json` + `pnpm-workspace.yaml` +
  `turbo.json` skeleton).

## 2026-09-25 — P06 · Admin app v0

- The JavaScript/TypeScript side of the monorepo is alive: root `package.json` +
  `pnpm-workspace.yaml` (`apps/*`, `packages/*`) + `turbo.json` (`build`/`dev`/`typecheck`), so
  `pnpm build` at the root builds every app through turbo (verified: `1 successful, 1 total`).
- New app `apps/admin/` (Next.js 16 + React 19 + TypeScript 5.9 + Tailwind v4, docs/03-FRONTEND.md):
  the app shell (sidebar sections, sticky header with the current screen, mobile drawer), an
  API-wired sign-in screen, an overview screen with live workspace totals and the account card,
  the pages list of the selected site (state filter, live/draft revision per row, reload, empty
  and error states) and the sites list.
- Same-origin API access: `next.config.ts` forwards `/api/*` to `OMNION_API_URL` (default
  `http://127.0.0.1:8080`), so the API's HttpOnly session cookie stays first-party and no CORS
  rule is needed; a deployment only has to route `/api/*` at the edge.
- Session handling is server-first. `proxy.ts` (Next.js 16's renamed `middleware.ts`) turns
  visitors without a session cookie away from panel routes with a `307` to `/login`, the root
  layout resolves the account through `GET /api/v1/me` with the request's own cookie, and the
  client provider starts from that answer — the browser never probes the session itself, which
  keeps the console clean and shows no flash of protected UI.
- Tailwind v4 with `source(none)` + explicit `@source` roots: the repository also carries a Rust
  `target/` tree, so automatic content detection stays off and the app declares its own folders.
- Proof: `pnpm install` (2 workspace projects) · `pnpm --filter @omnion/admin run typecheck`
  clean · `pnpm --filter @omnion/admin run build` green (7 routes; `ƒ Proxy (Middleware)`) ·
  root `pnpm build` → turbo `1 successful, 1 total` · the compiled stylesheet carries the
  utilities the screens use (`bg-canvas`, `text-muted`, `border-line`, `sm:grid-cols-3`,
  `hover:bg-canvas`, …). Live walk on `:3100` against a fresh database (`omnion_p06_live`,
  dropped afterwards) with the API on `:8080`: seeded one tenant → one site → two pages (one
  published with a newer draft, one draft-only) through the API, then drove the panel in a real
  browser (Playwright): anonymous `/` → **307** to `/login`; a wrong password shows the API's
  own refusal (`email or password is incorrect`); the bootstrap account signs in and lands on
  the overview with totals `1 site / 2 pages / 1 published`; the switcher lists the site; the
  pages list shows `/home · live v1 · draft v2` and `/about · not published · draft v1`; the
  state filter narrows the list to the published page; the sites list marks the selected site;
  sign-out clears the cookie and a guarded route answers with `/login`. **17/17 checks passed,
  0 console errors** (the only rejected request in the whole walk is the deliberate wrong
  password on `/api/v1/auth/login`). Screenshot: `/tmp/omnion_p06_pages.png`.
- Next: **P07 — Public web v0** (`apps/web` server-side renderer for published pages + the
  `themes/minimal` stub, `GET /:slug` renders).

## 2026-09-25 — P07 · Public web v0

- The platform now *serves* a site, not just manages one. `crates/identity` gained
  `find_site_by_global_key` (a key resolves platform-wide only when exactly one site carries it —
  keys are unique per organization), and `apps/api` gained the **public surface**
  `GET /api/v1/public/pages/{slug}`: unauthenticated, published revisions only, and no internal
  identifiers in the payload (site key + name · page slug/type/updated_at · revision
  number/title/body/summary/published_at).
- Site resolution is part of the request, documented in `apps/api/src/routes/public.rs`: `?site=`
  (a host when it contains a dot, otherwise a key) → `X-Forwarded-Host`/`Host` with the port
  dropped → the installation's only site when there is exactly one. An address that matches
  nothing answers `404` — and it answers the *same* `404` for "unknown" and "not published", so
  the public surface never discloses a draft. An address that is not a slug shape is a `404` too,
  never a `400`: the panel's validation rules stay in the panel.
- New JavaScript side: `packages/types` (the public content shapes — types-only), `packages/theme-sdk`
  (the theme contract: `SiteTheme`, `PageLayoutProps`, `ThemeManifest`, `defineTheme`),
  `themes/minimal` (manifest + `PageLayout` + stylesheet; warm cream/ink/terracotta palette with a
  light/dark pair), and `apps/web` — Next.js 16 renderer where `app/[[...slug]]/page.tsx` serves the
  site's `home` page at `/` and any published page at `/{slug}` (`force-dynamic`, so a publish is
  visible without a rebuild), `lib/api.ts` reads the API server-side and forwards the visitor's host
  as the site hint unless it is a loopback address, `lib/theme.ts` is the theme registry, and
  `lib/metadata.ts` emits title/description plus canonical and Open Graph URLs when
  `OMNION_SITE_URL` is set. `pnpm-workspace.yaml` now includes `themes/*`.
- Proof: `cargo fmt --all -- --check` clean · `cargo clippy --workspace --all-targets -- -D warnings`
  clean · `cargo test --workspace` → **145 passed** (api unit 33 incl. the new hint/host helpers ·
  public integration 2 · the rest unchanged) · `pnpm --filter @omnion/web run typecheck` clean ·
  `pnpm --filter @omnion/web run build` green (`ƒ /[[...slug]]` server-rendered on demand) · root
  `pnpm build` → turbo `2 successful, 2 total`.
- Live walk on a fresh database (`omnion_p07_live`, dropped afterwards; API on `:18090`, renderer on
  `:3200`): the draft answers `404` on the public surface; after publishing, the public read answers
  `200` through `?site=main`, through `Host: p07.omnion.test` and with no hint at all (single-site
  fallback); `GET /home` returns 7509 bytes of HTML containing the title, the body paragraph,
  `Powered by Omnion` and `data-theme="minimal"`; `GET /` serves the home page; `GET /` with
  `Host: p07.omnion.test` renders through the renderer's host forwarding; `/missing` and the
  draft-only `/soon` answer the not-found view; a revision published while the renderer kept running
  appeared in the HTML without a restart; the theme's stylesheet came back with its `--mn-accent`
  token.
- CI: the smoke step now also reads the published page through the public surface (by key and by the
  site's own domain), and it creates a page that is never published to assert the public `404`. The
  block was rehearsed locally against the built binary first (`SMOKE BLOCK REHEARSAL OK:
  public_title=Welcome to Omnion host_title=Welcome to Omnion draft=404`).
- CI run `36202889332` → **success**: `Rust — fmt · clippy · test` ✅ (the public integration suite
  ran in CI — `Running tests/public.rs` → `2 passed` — and the smoke log prints the public evidence:
  `public surface: title="Welcome to Omnion" via-key · "Welcome to Omnion" via-domain ·
  unpublished=404`) · `Infra — compose config` ✅.
- Next: **P08 — Media v0** (`crates/storage` S3/MinIO abstraction, upload endpoint, media table and
  the public serve path).
