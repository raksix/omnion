# Omnion — Monorepo Structure

> Captured from the owner's brief (2026-09-25, part 4). Decision: **a single GitHub repo
> (monorepo)**, laid out enterprise-grade from day one so that the repository stays manageable
> even if the project grows huge later.

## Repository layout

```text
omnion/
│
├── apps/
│   ├── api/                    # Rust + Axum
│   ├── admin/                  # Next.js Admin Panel
│   ├── web/                    # Next.js public site renderer
│   └── marketplace/            # Marketplace frontend
│
├── crates/
│   ├── core/                   # Omnion core
│   ├── auth/                   # Authentication
│   ├── identity/               # Users / Organizations
│   ├── permissions/            # RBAC / policies
│   ├── sites/                  # Multi-site
│   ├── content/                # CMS
│   ├── media/                  # Media management
│   ├── localization/           # i18n
│   ├── translation/            # Translation system
│   ├── themes/                 # Theme engine
│   ├── workflows/              # Workflow engine
│   ├── audit/                  # Audit logs
│   ├── notifications/          # Email/web notifications
│   ├── webhooks/               # Webhook system
│   ├── search/                 # Search abstraction
│   ├── storage/                # S3/MinIO abstraction
│   └── plugin-runtime/         # WASM plugin runtime
│
├── packages/
│   ├── ui/                     # Shared React UI
│   ├── types/                  # Shared TypeScript types
│   ├── api-client/             # Generated API client
│   ├── plugin-sdk/             # Plugin SDK
│   ├── theme-sdk/              # Theme SDK
│   └── eslint-config/
│
├── modules/
│   ├── cms/
│   ├── forms/
│   ├── blog/
│   ├── ecommerce/
│   ├── crm/
│   ├── hr/
│   └── documentation/
│
├── themes/
│   ├── corporate/
│   ├── tech/
│   ├── agency/
│   ├── startup/
│   ├── minimal/
│   ├── magazine/
│   ├── commerce/
│   ├── documentation/
│   ├── portfolio/
│   └── government/
│
├── plugins/
│   └── examples/
│
├── database/
│   ├── migrations/
│   ├── seeds/
│   └── fixtures/
│
├── infra/
│   ├── docker/
│   ├── compose/
│   ├── kubernetes/
│   ├── helm/
│   └── terraform/
│
├── tools/
│   ├── cli/
│   ├── codegen/
│   └── generators/
│
├── docs/
│   ├── architecture/
│   ├── development/
│   ├── plugins/
│   ├── themes/
│   ├── deployment/
│   ├── api/
│   └── enterprise/
│
├── tests/
│   ├── integration/
│   ├── e2e/
│   ├── security/
│   └── performance/
│
├── .github/
│   ├── workflows/
│   │   ├── ci.yml
│   │   ├── test.yml
│   │   ├── security.yml
│   │   ├── release.yml
│   │   └── docker.yml
│   ├── ISSUE_TEMPLATE/
│   └── PULL_REQUEST_TEMPLATE.md
│
├── docker-compose.yml
├── docker-compose.dev.yml
├── Dockerfile
├── Cargo.toml
├── Cargo.lock
├── package.json
├── pnpm-workspace.yaml
├── turbo.json
├── .env.example
├── .gitignore
├── LICENSE
├── SECURITY.md
├── CONTRIBUTING.md
├── CODE_OF_CONDUCT.md
├── CHANGELOG.md
└── README.md
```

## One refinement: keep `modules/` and `crates/` distinct

- `crates/` — Omnion's infrastructure and core.
- `modules/` — features the user can turn on and off.

Example:

```text
crates/
└── content/

modules/
├── cms/
├── blog/
├── ecommerce/
└── crm/
```

This way, CRM does not get glued into the Core.

## Rust workspace

Root `Cargo.toml`:

```toml
[workspace]
resolver = "2"

members = [
    "apps/api",
    "crates/*"
]
```

`apps/api` is only the HTTP layer:

```text
apps/api
└── src/
    ├── main.rs
    ├── routes/
    ├── middleware/
    └── state.rs
```

The real work lives in `crates/`. This way, binaries like these can be added later:

```text
omnion-worker
omnion-cli
omnion-migrate
```

## Next.js side

Admin:

```text
apps/admin/
├── app/
├── components/
├── features/
├── hooks/
├── lib/
└── public/
```

Public frontend:

```text
apps/web/
├── app/
├── components/
├── themes/
├── lib/
└── public/
```

Here, theme and content are **fully separated**.

## 10 default themes

Live directly in the repo:

```text
themes/
├── corporate/
├── tech/
├── agency/
├── startup/
├── minimal/
├── magazine/
├── commerce/
├── documentation/
├── portfolio/
└── government/
```

These are not marketplace imports — they are **Omnion's official default themes**.

## Docker structure

Development:

```text
infra/compose/
├── docker-compose.dev.yml
├── postgres.yml
├── redis.yml
└── minio.yml
```

Production:

```text
infra/
├── docker/
│   ├── api.Dockerfile
│   ├── admin.Dockerfile
│   └── web.Dockerfile
│
├── kubernetes/
│   ├── namespace.yaml
│   ├── api.yaml
│   ├── admin.yaml
│   ├── web.yaml
│   └── ingress.yaml
│
└── helm/
    └── omnion/
```

Users start it with:

```bash
docker compose up -d
```

## GitHub Actions

On every PR, automatically:

```text
PR
 │
 ├── Rust fmt
 ├── Clippy
 ├── Rust tests
 ├── TypeScript check
 ├── ESLint
 ├── Frontend tests
 ├── E2E
 ├── Security scan
 └── Build
```

Release:

```text
GitHub Release
      │
      ├── Docker image
      ├── CLI binaries
      ├── Helm chart
      └── Release artifacts
```

## Makefile / CLI

Users should not have to fight Docker commands by hand:

```bash
omnion dev
omnion build
omnion test
omnion migrate
omnion seed
omnion plugin create
omnion theme create
omnion doctor
```

`omnion doctor` is especially nice:

```text
Omnion Doctor

✓ PostgreSQL
✓ Redis
✓ Storage
✓ Database migrations
✓ Configuration
✓ Plugin runtime

System is healthy.
```

## The most critical rule

**One repo — but nothing gets entangled.** Target dependency graph:

```text
                    apps
                     │
          ┌──────────┴──────────┐
          ▼                     ▼
       Admin/Web              API
          │                     │
          └──────────┬──────────┘
                     ▼
                 SDK / Types
                     │
                     ▼
                  Core
                     │
        ┌────────────┼────────────┐
        ▼            ▼            ▼
    PostgreSQL     Redis         S3
```

And the modules sit on top of Core:

```text
Core
 │
 ├── CMS
 ├── CRM
 ├── HR
 ├── E-Commerce
 └── Documentation
```

Built this way, Omnion starts in a single GitHub repo — and stays manageable even if it grows
enormous later. Enterprise-grade monorepo thinking from the very beginning.
