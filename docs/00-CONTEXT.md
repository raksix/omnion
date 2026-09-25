# Omnion — Project Context

Living context document: what Omnion is, the owner's directives, and open questions.
Updated continuously — new directives/decisions get appended here as they arrive.
Chat is Turkish; this document is English.

_Last updated: 2026-09-25_

## Phase

- **Context gathering. No implementation — yet.** The owner paused building on 2026-09-25
  ("we are not building the project now; we'll gather context"). Do not scaffold
  stack choices, features or code until the owner asks to start.

## What Omnion is

- Positioning (owner, 2026-09-25): **"Omnion is an open-source enterprise application platform
  with a powerful CMS at its core."**
- Omnion is a **CMS (content management system) panel**.
- Public framing: professional product language only — the project is presented as the
  platform/CMS described above, nothing outside that framing.
- Target architecture & feature vision: [`docs/01-VISION.md`](01-VISION.md) — corporate,
  multi-tenant, headless-capable platform/CMS.
- Technical architecture & stack direction: [`docs/02-ARCHITECTURE.md`](02-ARCHITECTURE.md) —
  modular monolith (Rust/Axum), Docker → Kubernetes deployment path, WASM plugin runtime.
- Frontend & theme system: [`docs/03-FRONTEND.md`](03-FRONTEND.md) — Next.js/React/TS admin
  panel + public theme engine, 10 default themes, block editor, Theme Builder, theme SDK.
- Monorepo layout: [`docs/04-MONOREPO.md`](04-MONOREPO.md) — single repo; apps/crates/packages/
  modules/themes/infra/tools; Rust workspace; CI pipeline and the `omnion` CLI.
- Versioning & release management: [`docs/05-VERSIONING.md`](05-VERSIONING.md) — SemVer per
  layer, Git flow + releases, content revisions with diff/restore, reversible migrations,
  plugin/theme compatibility, feature flags, environment promotion.
- Feature request pool: [`docs/requests/`](requests/) — 24 individual requests (REQ-001…REQ-024),
  one per platform feature layer; mobile app intentionally not captured.
- AI Hub design: [`docs/06-AI-HUB.md`](06-AI-HUB.md) — providers/registry/router, agent runtime,
  tool + permission + approval + audit chain, RAG, cost manager, data guard.
- IAM design: [`docs/07-IAM.md`](07-IAM.md) — custom roles, RBAC + ABAC, policy engine, scopes,
  permission simulator, service accounts.

## Hard rules (owner directives)

1. **General-audience, professional wording only.** This repository is public. Every word that
   ships with it — README, docs, code, comments, commit messages, repo description, asset
   names — must be plain, neutral CMS/product language appropriate for a general audience.
   Nothing suggestive, edgy or informal goes in, even as a placeholder or joke.
2. **Public repo hygiene.** Never commit secrets, credentials, API keys or heavy media
   files; media belongs on the app's own storage/CDN.
3. **Language split.** Chat in Turkish; documentation, code comments and commit messages
   in English.
4. **Commits.** Small, atomic, imperative, English. Push immediately after every change.
5. **Context first.** Capture directives in this file as they arrive; build only when the
   owner asks to start.

## Directives log (append-only)

| Date | Directive (owner) | Applied as |
|---|---|---|
| 2026-09-25 | Omnion is a general-audience CMS panel; the public repo must never carry language outside that framing; clean up any wording that violates it and re-push. | README wording, commit history and repository description cleaned; history rewritten; the previous repo was privatized as `raksix/omnion-legacy`; a fresh public repo was published and pushed. Rule 1 recorded. |
| 2026-09-25 | "We are not building the project now — we'll gather context; just save my directives into docs." | This file created; build paused. |
| 2026-09-25 | Shared the target architecture vision: corporate, multi-tenant, headless-capable CMS — Odoo-style module system, LDAP/AD/SAML/OAuth2/OIDC + MFA, theme system + theme API, i18n + Translation Center + Translation Memory, Content Type Builder, workflow + audit log, RBAC permissions, multi-site vs multi-tenant split, API-first (REST/GraphQL/Webhooks/Events), event system. | Captured as [`docs/01-VISION.md`](01-VISION.md); pointer added above. |
| 2026-09-25 | Fixed the technical direction: modular monolith (no microservices on day one), preferred stack Rust + Axum + Tokio + SQLx + PostgreSQL + Redis + S3/MinIO + Next.js/React/TS + Docker/Kubernetes + OpenTelemetry + WASM plugin runtime; Docker Compose → Helm chart deployment path; versioned API (`/api/v1`) + OpenAPI SDK generation; positioning as an open-source enterprise application platform with a powerful CMS at its core. | Captured as [`docs/02-ARCHITECTURE.md`](02-ARCHITECTURE.md); positioning + pointers + open questions updated. |
| 2026-09-25 | Fixed the frontend direction: Next.js + React + TypeScript for both the admin panel and public sites (separate apps); WordPress-style theme system with 10 high-quality default themes, visual Theme Builder, Gutenberg-style block editor, strict theme/content separation, `omnion create-theme` SDK + theme API, shared `@omnion/ui` kit (theme-overridable), and a future Omnion Marketplace. | Captured as [`docs/03-FRONTEND.md`](03-FRONTEND.md); pointer added. |
| 2026-09-25 | Decided on a single GitHub repo (monorepo): apps (api/admin/web/marketplace), crates (core + infrastructure), packages (SDK/types/UI), modules (toggleable features), official themes, infra (docker/compose/k8s/helm/terraform), tools/cli, database migrations. Rust workspace — `apps/api` is only the HTTP layer, real work in `crates/`. Critical rule: one repo but nothing entangled (apps → SDK/types → core → infra). | Captured as [`docs/04-MONOREPO.md`](04-MONOREPO.md); pointer added. |
| 2026-09-25 | Fixed the versioning & release strategy: SemVer at every layer (core/modules/plugins/themes/content revisions); Git flow (main/develop/feature/release) → tag → GitHub Release → Docker images; CMS content revision history with compare/restore + block-level diff; draft/published + scheduled publishing; reversible DB migrations (backup → migrate → health check); plugin manifests with compatibility ranges + dependency resolver; system update manager with rollback (non-destructive/expand-contract migrations only); audit history; config versioning; feature flags (env/org/site scoped); same-artifact environment promotion; repo gains `versioning/` + `release/` directories and a `VERSION` file. | Captured as [`docs/05-VERSIONING.md`](05-VERSIONING.md); 04 tree extended; pointer added. |
| 2026-09-25 | Shared the full platform feature pool (25 layers) and asked for each item to be captured as a **separate request** — mobile app excluded. Guiding principle: one platform, keep the Core thin (features live under `modules/`). | Captured as [`docs/requests/`](requests/) — REQ-001…REQ-024 with an index (`README.md`). |
| 2026-09-25 | Shared the deep AI Hub design: user-extensible AI providers (incl. OpenAI-compatible custom), provider abstraction, model registry + model router, AI agents that operate the Omnion API through tools with separate RBAC permissions, approval flow for dangerous actions, action previews, AI memory + RAG knowledge base, agents marketplace, multi-agent orchestrator, cost manager per org/site, AI audit logs, privacy/data guard (PII masking), local-AI (Ollama/vLLM) support, and the AI Hub admin screen. Core security principle: `AI Agent → Tool → Permission → Approval → Audit`. | Captured as [`docs/06-AI-HUB.md`](06-AI-HUB.md); linked from REQ-001. |
| 2026-09-25 | Shared the deep IAM design (enterprise RBAC/ABAC): fully custom roles; granular permissions; Discord-style role hierarchy with priority values; role inheritance; explicit allow/deny precedence; scopes (global/org/site/department/module/resource); organization → site → team model; groups; multi-role users with effective permissions; resource-level permissions; ABAC policies + visual Policy Builder; special Owner role (audited); the same role system for AI agents; service accounts/API keys; temporary roles; approval-based permissions; permission simulator; role audit history; permission-safety invariants. Crate split: `identity/`, `permissions/`, `policy-engine/`, `authorization/`, `audit/`. | Captured as [`docs/07-IAM.md`](07-IAM.md); linked from REQ-006; 04 crates list extended. |

## Open questions (to resolve while gathering context)

- Open-source license choice (positioning is open-source; license not selected yet).
- Build order: which core pieces and modules get built first once implementation starts.
- Scope of the first deliverable (panel only vs. panel + a theme/frontend).
- Naming/branding details beyond "Omnion".
- SaaS productization timing (the multi-tenant model already exists from day one).
- Full technical blueprint details (folder layout, DB schema, multi-tenant model, plugin API,
  theme API, LDAP architecture, translation system, Docker/K8s manifests) — stack direction is
  set ([`docs/02-ARCHITECTURE.md`](02-ARCHITECTURE.md)); the detailed blueprint is the
  next-step deliverable.
- Omnion Marketplace scope/timing (third-party theme & plugin distribution) — after the core
  platform is stable.

## How to work with this file

1. Apply the directive.
2. Add a row to the directives log.
3. If it changes how we work, update "Hard rules".
