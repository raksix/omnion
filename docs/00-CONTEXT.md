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
