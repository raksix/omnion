# Omnion

**Omnion** is an open-source enterprise application platform with a powerful CMS at its core:
multi-tenant content, identity and permissions, media, workflows, automation, events and an AI
hub on one Core.

> **Status: early build.** The foundation (P00–P14) is in place and CI runs it end to end —
> API, identity, IAM, tenancy, content, media, workflows, onboarding, AI hub, events/webhooks
> and automation, plus the admin panel and the public renderer. The business suite, the
> marketplace and the deployment phases arrive later; the queue lives in
> [`docs/BUILD-BACKLOG.md`](docs/BUILD-BACKLOG.md) and progress is recorded in
> [`docs/BUILD-LOG.md`](docs/BUILD-LOG.md).

## Quickstart

Prerequisites: **Rust** (stable, via [rustup](https://rustup.rs)) · **Node.js ≥ 20.9** with
**pnpm 11** (`corepack enable`) · **Docker** with Compose.

### 1. Start the development stack

```bash
docker compose -f infra/compose/docker-compose.dev.yml up -d
```

| Service | Address | Credentials |
|---|---|---|
| PostgreSQL | `127.0.0.1:5433` | `omnion` / `omnion` (db `omnion`) |
| Redis | `127.0.0.1:6380` | — |
| MinIO (S3-compatible storage) | `http://127.0.0.1:9000` · console `:9001` | `omnion` / `omnion-dev-secret` |
| Mailpit (local SMTP sink) | SMTP `127.0.0.1:1025` · inbox `http://127.0.0.1:8025` | — |

Host ports are overridable through `OMNION_*` variables (`OMNION_POSTGRES_PORT`, …) — see
[`infra/compose/docker-compose.dev.yml`](infra/compose/docker-compose.dev.yml).

### 2. Run the API

```bash
cargo run -p omnion-api                       # http://127.0.0.1:8080
curl http://127.0.0.1:8080/healthz            # {"ok":true,…}
curl http://127.0.0.1:8080/readyz             # database + redis
```

### 3. First run

A fresh installation has no owner yet. Set it up from the terminal:

```bash
cargo run -p omnion-cli -- setup              # owner → organization → first site → theme
cargo run -p omnion-cli -- doctor             # configuration · database · migrations · redis · storage
```

…or open the admin panel (step 4): an installation that has not been set up lands on its
`/setup` wizard instead of the sign-in screen.

### 4. Admin panel

```bash
pnpm install
pnpm --filter @omnion/admin dev               # http://127.0.0.1:3100
```

The panel forwards `/api/*` to the API (`OMNION_API_URL`, default `http://127.0.0.1:8080`), so
the session cookie stays first-party and no CORS rule is needed.

### 5. Public renderer

```bash
pnpm --filter @omnion/web dev                 # http://127.0.0.1:3200
```

It renders **published** pages of the resolved site through the theme engine
([`themes/minimal`](themes/minimal) ships with the repository).

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                        # unit + integration (compose stack required)
pnpm typecheck
pnpm build                                    # admin + public renderer
```

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs exactly these on every push:
formatting, clippy, the test suite against PostgreSQL + Redis + MinIO service containers, the
smoke walks (identity, tenancy, content, media, workflows, onboarding, AI hub against a mock
provider, signed webhook deliveries), the TypeScript install/typecheck/build, and the
development compose stack validation.

## Repository layout

| Path | What |
|---|---|
| `apps/api` | The HTTP layer — Rust + Axum. Thin: routes, state, background runners |
| `apps/admin` | Administration panel — Next.js + React + TypeScript + Tailwind |
| `apps/web` | Public site renderer — Next.js on top of the theme engine |
| `crates/` | The Core — infrastructure (`core`) and feature crates (`identity`, `permissions`, `content`, `media`, `workflows`, `events`, `automation`, `ai-hub`, `onboarding`, `audit`, `storage`) |
| `packages/` | Shared contracts — `@omnion/types`, `@omnion/theme-sdk` |
| `themes/` | Official themes (`minimal`) |
| `database/migrations` | SQLx migrations (reversible, ordered) |
| `infra/compose` | The development stack; `infra/mocks` — development/CI mocks |
| `tools/cli` | The `omnion` CLI (`setup`, `doctor`, `migrate`) |
| `docs/` | The specification set — vision, architecture, frontend, monorepo, versioning, AI hub, IAM, business suite |

## Documentation

| Document | Contents |
|---|---|
| [`docs/00-CONTEXT.md`](docs/00-CONTEXT.md) | Project context, owner directives, hard rules |
| [`docs/01-VISION.md`](docs/01-VISION.md) | Target architecture and feature vision |
| [`docs/02-ARCHITECTURE.md`](docs/02-ARCHITECTURE.md) | Stack, layering, deployment path |
| [`docs/03-FRONTEND.md`](docs/03-FRONTEND.md) | Admin panel, themes, block editor, Theme SDK |
| [`docs/04-MONOREPO.md`](docs/04-MONOREPO.md) | Repository layout and CI pipeline |
| [`docs/05-VERSIONING.md`](docs/05-VERSIONING.md) | Versioning, releases, migrations, feature flags |
| [`docs/06-AI-HUB.md`](docs/06-AI-HUB.md) | Providers, agents, tools, approvals, audit |
| [`docs/07-IAM.md`](docs/07-IAM.md) | Roles, permissions, scopes, policy engine |
| [`docs/08-BUSINESS-SUITE.md`](docs/08-BUSINESS-SUITE.md) | The Odoo-style module ecosystem |
| [`docs/09-N8N-TEARDOWN.md`](docs/09-N8N-TEARDOWN.md) | Automation engine lessons from n8n |
| [`docs/BUILD-BACKLOG.md`](docs/BUILD-BACKLOG.md) | The ordered build queue and its status |

## Conventions

- Documentation, code comments and commit messages in English; commits are small, atomic and
  imperative, pushed immediately.
- **Public repository** — never commit secrets, credentials or large media files; media belongs
  on the application's own storage/CDN.
- The Core stays thin: business features live under `modules/` as they arrive, while
  `crates/core` remains infrastructure only.

## License

Not selected yet. The project is developed in the open; a license will be chosen before the
first release.
