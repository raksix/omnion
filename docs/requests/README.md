# Omnion — Feature Requests (inbox)

> The platform feature pool, captured as **individual requests** from the owner's brief
> (2026-09-25; extended the same day with the platform periphery + headline features —
> REQ-025…REQ-050). One request per feature layer; each gets implemented (or re-scoped)
> later — they are capture, not commitment yet.
>
> Status convention: `pending` → `in-progress` → `done (<commit>)`.

## Guiding principle

Everything fits into **one platform** — not "Omnion CMS" but **Omnion Platform**:

```text
                         OMNION
                            │
       ┌────────────────────┼────────────────────┐
       │                    │                    │
      CMS                 AI                Commerce
       │                    │                    │
       ├── Pages            ├── AI Writer        ├── Products
       ├── Blog             ├── RAG              ├── Orders
       ├── Media            ├── Agents           ├── Inventory
       ├── Forms            └── Providers        └── Payments
       │
       ├──────────────┐
       │              │
    Workflow        Automation
       │              │
       └───────┬──────┘
               │
         Plugin System
               │
        Marketplace
               │
      ┌────────┴────────┐
      │                 │
   Themes            Integrations
      │                 │
      └────────┬────────┘
               │
          Omnion Core
               │
      PostgreSQL / Redis
         S3 / Kubernetes
```

And the most critical planning rule: **do not bloat the Core.** CMS, CRM, Commerce, HR,
Helpdesk and AI live under `modules/`; the Core only provides the infrastructure they sit on.
That keeps the system both huge and manageable.

**Owner's final synthesis (2026-09-25):** WordPress's CMS side + Odoo's business side + n8n's
automation side + an AI agent platform + a low-code app builder + enterprise IAM + a
marketplace — all connected by the **Omnion Core**.

## Index

| # | Request | Layer | Status |
|---|---|---|---|
| REQ-001 | [AI Engine](REQ-001-ai-engine.md) | `module (`modules/ai`)` | pending |
| REQ-002 | [Global Search Engine](REQ-002-global-search.md) | `core (`crates/search`) + admin UI` | done |
| REQ-003 | [Automation Engine](REQ-003-automation-engine.md) | `core engine (`crates/workflows`) + admin UI` | pending |
| REQ-004 | [Visual Workflow Builder](REQ-004-visual-workflow-builder.md) | ``apps/admin` + `crates/workflows`` | pending |
| REQ-005 | [Organization / Tenant System](REQ-005-organization-tenant-system.md) | `core (`crates/identity`)` | pending |
| REQ-006 | [Advanced IAM](REQ-006-advanced-iam.md) | `core (`crates/auth`, `crates/permissions`)` | pending |
| REQ-007 | [Analytics](REQ-007-analytics.md) | `module (`modules/analytics`)` | pending |
| REQ-008 | [Commerce Engine](REQ-008-commerce-engine.md) | `module (`modules/ecommerce`)` | pending |
| REQ-009 | [Helpdesk / Ticket System](REQ-009-helpdesk-tickets.md) | `module (`modules/helpdesk`)` | pending |
| REQ-010 | [Enterprise File Manager](REQ-010-enterprise-file-manager.md) | `core (`crates/media`) + admin UI` | pending |
| REQ-011 | [CDN / Edge System](REQ-011-cdn-edge.md) | `platform / infra` | pending |
| REQ-012 | [Security Center](REQ-012-security-center.md) | `core + admin UI` | pending |
| REQ-013 | [Backup Center](REQ-013-backup-center.md) | `core + admin UI` | pending |
| REQ-014 | [System Health](REQ-014-system-health.md) | `core + admin UI` | pending |
| REQ-015 | [Integration Hub](REQ-015-integration-hub.md) | `module (`modules/integrations`)` | pending |
| REQ-016 | [Webhook + Event Bus](REQ-016-webhook-event-bus.md) | `core (`crates/webhooks`)` | pending |
| REQ-017 | [Sandbox / Staging](REQ-017-sandbox-staging.md) | `platform` | pending |
| REQ-018 | [Preview System](REQ-018-preview-system.md) | ``apps/web` + core` | pending |
| REQ-019 | [Headless CMS](REQ-019-headless-cms.md) | `core API (`crates/content` + `apps/api`)` | pending |
| REQ-020 | [Globalization](REQ-020-globalization.md) | `core (`crates/localization`)` | pending |
| REQ-021 | [Notification Center](REQ-021-notification-center.md) | `core (`crates/notifications`) + admin UI` | pending |
| REQ-022 | [Developer Portal](REQ-022-developer-portal.md) | ``apps/admin` + core` | pending |
| REQ-023 | [Marketplace Expansion](REQ-023-marketplace-expansion.md) | ``apps/marketplace`` | pending |
| REQ-024 | [Deployment Center](REQ-024-deployment-center.md) | ``apps/admin` + infra` | pending |
| REQ-025 | [App Builder / No-Code Builder](REQ-025-app-builder.md) | `platform (`apps/admin` + Studio)` | pending |
| REQ-026 | [Dynamic Data Model](REQ-026-dynamic-data-model.md) | `core (dynamic entity layer)` | pending |
| REQ-027 | [Dashboard Builder](REQ-027-dashboard-builder.md) | ``apps/admin` (Studio)` | pending |
| REQ-028 | [BI / Reporting Engine](REQ-028-bi-reporting.md) | `module (`modules/reporting`)` | pending |
| REQ-029 | [PDF / Document Generation](REQ-029-pdf-document-generation.md) | `core service (documents)` | pending |
| REQ-030 | [E-signature](REQ-030-e-signature.md) | `module (`modules/esignature`)` | pending |
| REQ-031 | [Records / Data Import-Export Center](REQ-031-import-export-center.md) | `core (data import/export) + admin UI` | pending |
| REQ-032 | [Universal Command Center](REQ-032-command-center.md) | ``apps/admin` + core search` | done |
| REQ-033 | [Internal Developer Platform](REQ-033-internal-developer-platform.md) | ``apps/admin` + SDKs` | pending |
| REQ-034 | [Sandbox Environments](REQ-034-sandbox-environments.md) | `platform (org-scoped environments)` | pending |
| REQ-035 | [Edge / Multi-region](REQ-035-edge-multi-region.md) | `platform / infra` | pending |
| REQ-036 | [Offline / Air-gapped Mode](REQ-036-air-gapped-mode.md) | `platform / infra` | pending |
| REQ-037 | [Secrets Manager](REQ-037-secrets-manager.md) | `core (secrets) + integrations` | pending |
| REQ-038 | [Compliance Center](REQ-038-compliance-center.md) | `core + admin UI` | pending |
| REQ-039 | [Advanced Audit](REQ-039-advanced-audit.md) | `core (`crates/audit`)` | pending |
| REQ-040 | [API Gateway](REQ-040-api-gateway.md) | `core (`apps/api` edge)` | pending |
| REQ-041 | [Real-time Platform](REQ-041-realtime-platform.md) | `core (WS/SSE layer) + admin` | pending |
| REQ-042 | [AI Copilots in Every Module](REQ-042-ai-copilots.md) | `AI Hub + per-module UI` | pending |
| REQ-043 | [White-label](REQ-043-white-label.md) | `platform` | pending |
| REQ-044 | [Package / App Installer](REQ-044-package-installer.md) | `platform (module manager)` | pending |
| REQ-045 | [AI App Builder *(headline)*](REQ-045-ai-app-builder.md) | `AI Hub × App Builder` | pending |
| REQ-046 | [AI Workflow Builder *(headline)*](REQ-046-ai-workflow-builder.md) | `AI Hub × workflow engine` | pending |
| REQ-047 | [AI Admin *(headline)*](REQ-047-ai-admin.md) | `AI Hub × audit/logs` | pending |
| REQ-048 | [App Marketplace *(headline)*](REQ-048-app-marketplace.md) | ``apps/marketplace`` | pending |
| REQ-049 | [Omnion Studio *(headline)*](REQ-049-omnion-studio.md) | `new `apps/studio`` | pending |
| REQ-050 | [First-run Onboarding / Setup Wizard](REQ-050-first-run-onboarding.md) | `platform (`apps/admin` + CLI)` | delivered |
| REQ-051 | [CRM](REQ-051-crm.md) | `module (`modules/crm`)` | pending |
| REQ-052 | [Sales & Quotes](REQ-052-sales.md) | `module (`modules/sales`)` | pending |
| REQ-053 | [Inventory & Warehouse](REQ-053-inventory.md) | `module (`modules/inventory`)` | pending |
| REQ-054 | [Accounting](REQ-054-accounting.md) | `module (`modules/accounting`)` | pending |
| REQ-055 | [HR](REQ-055-hr.md) | `module (`modules/hr`)` | pending |
| REQ-056 | [Projects & Tasks](REQ-056-projects.md) | `module (`modules/projects`)` | pending |
| REQ-057 | [Calendar & Appointments](REQ-057-calendar.md) | `module (`modules/calendar`)` | pending |
| REQ-058 | [Documents & Knowledge](REQ-058-documents.md) | `module (`modules/documents`)` | pending |
| REQ-059 | [Approvals](REQ-059-approvals.md) | `module (`modules/approvals`)` | pending |
| REQ-060 | [Marketing](REQ-060-marketing.md) | `module (`modules/marketing`)` | pending |
| REQ-061 | [Manufacturing](REQ-061-manufacturing.md) | `module (`modules/manufacturing`)` | pending |
| REQ-062 | [Themes & Theme Builder](REQ-062-themes.md) | `platform (`themes/*` + admin)` | pending |
| REQ-063 | [Block System & Page Builder](REQ-063-page-builder.md) | `platform (`apps/admin` + `crates/content`)` | pending |
| REQ-064 | [CMS Depth Pack](REQ-064-cms-depth.md) | `platform (core + admin + web)` | pending |
| REQ-065 | [Identity Providers & SSO](REQ-065-identity-providers-and-sso.md) | `core (`crates/identity`, `crates/auth`) + admin` | pending |
| REQ-066 | [MFA, Passkeys & Device Trust](REQ-066-mfa-passkeys-and-device-trust.md) | `core (`crates/identity`) + admin` | pending |
| REQ-067 | [Role Management UI](REQ-067-role-management-ui.md) | `admin (`apps/admin`) + core (`crates/permissions`)` | pending |
| REQ-068 | [Permission Catalogue & Effective Permissions](REQ-068-permission-catalogue-and-effective-permissions.md) | `core (`crates/permissions`) + admin` | pending |
| REQ-069 | [Policy Engine (ABAC)](REQ-069-policy-engine-abac.md) | `core (`crates/policy-engine`) + admin` | pending |
| REQ-070 | [Scopes & Resource-Level Permissions](REQ-070-scopes-and-resource-permissions.md) | `core (`crates/permissions`) + admin` | pending |
| REQ-071 | [Groups & Teams](REQ-071-groups-and-teams.md) | `core (`crates/identity`) + admin` | pending |
| REQ-072 | [Service Accounts & API Keys](REQ-072-service-accounts-and-api-keys.md) | `core (`crates/identity`) + admin` | pending |
| REQ-073 | [Temporary & Approval-Based Access](REQ-073-temporary-and-approval-based-access.md) | `core (`crates/permissions`) + admin` | pending |
| REQ-074 | [Permission Simulator & Authorization Diagnostics](REQ-074-permission-simulator.md) | `admin (`apps/admin`) + core (`crates/authorization`)` | pending |
| REQ-075 | [Appearance & Themes Screen](REQ-075-appearance-and-themes-screen.md) | `admin (`apps/admin`) + themes` | pending |
| REQ-076 | [Theme Builder UI](REQ-076-theme-builder-ui.md) | `admin (`apps/admin`) + themes` | pending |
| REQ-077 | [Revision History UI](REQ-077-revision-history-ui.md) | `admin (`apps/admin`) + core (`crates/content`)` | pending |
| REQ-078 | [System Updates Center](REQ-078-system-updates-center.md) | `admin (`apps/admin`) + infra` | pending |
| REQ-079 | [AI Command Center (cross-module jobs)](REQ-079-ai-command-center.md) | `admin (`apps/admin`) + ai` | pending |
| REQ-080 | [Admin Activity Timeline](REQ-080-admin-activity-timeline.md) | `admin (`apps/admin`) + core (`crates/audit`)` | pending |
| REQ-081 | [Frontend Packages & UI Kit](REQ-081-frontend-packages-and-ui-kit.md) | ``packages/*`` | pending |
| REQ-082 | [Ten Default Themes Pack](REQ-082-ten-default-themes.md) | `themes/*` | pending |
| REQ-083 | [Theme Sections & Slots Library](REQ-083-theme-sections-and-slots.md) | `themes/* + admin` | pending |
| REQ-084 | [Theme SDK & Packaging](REQ-084-theme-sdk-and-packaging.md) | ``packages/theme-sdk` + CLI` | pending |
| REQ-085 | [Design Quality Bar & Accessibility Gate](REQ-085-design-quality-bar.md) | `platform + themes` | pending |
| REQ-086 | [Workflow Editor Canvas](REQ-086-workflow-editor-canvas.md) | `admin (`apps/admin`) + `crates/workflows`` | pending |
| REQ-087 | [Node Library & Credential Catalog](REQ-087-node-library-and-credentials.md) | ``crates/workflows` + plugins` | pending |
| REQ-088 | [Core Node Families](REQ-088-core-node-families.md) | ``crates/workflows`` | pending |
| REQ-089 | [Triggers (webhook, schedule, polling)](REQ-089-triggers.md) | ``crates/workflows` + `crates/events`` | pending |
| REQ-090 | [Wait, Resume & Human-in-the-Loop](REQ-090-wait-resume-human-in-the-loop.md) | ``crates/workflows`` | pending |
| REQ-091 | [Execution Engine Hardening](REQ-091-execution-engine-hardening.md) | ``crates/workflows`` | pending |
| REQ-092 | [Expressions, Data Mapping & Variables](REQ-092-expressions-and-variables.md) | ``crates/workflows`` | pending |
| REQ-093 | [Execution History & Debugging UI](REQ-093-execution-history-and-debugging.md) | `admin + `crates/workflows`` | pending |
| REQ-094 | [Workflow Templates Gallery](REQ-094-workflow-templates-gallery.md) | `admin + `crates/workflows`` | pending |
| REQ-095 | [Workflow Versioning, Sharing & Permissions](REQ-095-workflow-versioning-and-sharing.md) | ``crates/workflows` + `crates/permissions`` | pending |
| REQ-096 | [Queue Mode & Scaling](REQ-096-queue-mode-and-scaling.md) | `infra + `crates/workflows`` | pending |
| REQ-097 | [AI Provider Runtime & Local Models](REQ-097-ai-provider-runtime.md) | ``crates/ai-hub`` | pending |
| REQ-098 | [Model Registry & Router](REQ-098-model-registry-and-router.md) | ``crates/ai-hub`` | pending |
| REQ-099 | [Agent Runtime & Tool Loop](REQ-099-agent-runtime.md) | ``crates/ai-hub`` | pending |
| REQ-100 | [AI Tool System & Permission Matrix](REQ-100-ai-tool-system.md) | ``crates/ai-hub` + `crates/permissions`` | pending |
| REQ-101 | [AI Approvals & Action Preview](REQ-101-ai-approvals-and-action-preview.md) | ``crates/ai-hub` + admin` | pending |
| REQ-102 | [AI Memory & Knowledge Base (RAG)](REQ-102-ai-memory-and-knowledge.md) | ``crates/ai-hub`` | pending |
| REQ-103 | [Per-Module AI Copilots](REQ-103-module-copilots.md) | `modules + `crates/ai-hub`` | pending |
| REQ-104 | [AI Cost Manager & AI Logs](REQ-104-ai-cost-manager-and-logs.md) | ``crates/ai-hub` + admin` | pending |
| REQ-105 | [AI Data Guard (PII protection)](REQ-105-ai-data-guard.md) | ``crates/ai-hub`` | pending |
| REQ-106 | [Local & Air-gapped AI Mode](REQ-106-local-and-air-gapped-ai.md) | ``crates/ai-hub` + infra` | pending |
| REQ-107 | [Agent Evals & Telemetry](REQ-107-agent-evals-and-telemetry.md) | ``crates/ai-hub`` | pending |
| REQ-108 | [MCP Server & Computer Use](REQ-108-mcp-server-and-computer-use.md) | ``crates/ai-hub`` | pending |
| REQ-109 | [Content Type Builder](REQ-109-content-type-builder.md) | `core (`crates/content`) + admin` | pending |
| REQ-110 | [Editorial Workflow & Content Lifecycle](REQ-110-editorial-workflow.md) | `core (`crates/content`) + admin` | pending |
| REQ-111 | [Diff Engine (text, asset, block)](REQ-111-diff-engine.md) | `core (`crates/content`)` | pending |
| REQ-112 | [Configuration Versioning & Restore](REQ-112-configuration-versioning.md) | `core + admin` | pending |
| REQ-113 | [Per-Site Configuration Surface](REQ-113-per-site-configuration.md) | `admin + core (`crates/sites`)` | pending |
| REQ-114 | [Translation Engine & Translation Memory](REQ-114-translation-engine-and-memory.md) | `core (`crates/translation`) + admin` | pending |
| REQ-115 | [SEO Intelligence & AI SEO Agent](REQ-115-seo-intelligence.md) | `modules/seo + `crates/ai-hub`` | pending |
| REQ-116 | [Blog / Magazine Module](REQ-116-blog-module.md) | `modules/blog + themes` | pending |
| REQ-117 | [Forms → CRM Lead Pipeline](REQ-117-forms-to-crm-pipeline.md) | `modules/website + modules/crm` | pending |
| REQ-118 | [Storefront & Checkout](REQ-118-storefront-and-checkout.md) | `modules/ecommerce + apps/web` | pending |
| REQ-119 | [Notification Channels & Templates](REQ-119-notification-channels-and-templates.md) | `core (`crates/notifications`) + admin` | pending |
| REQ-120 | [Multi-Site Operations](REQ-120-multi-site-operations.md) | `admin + core (`crates/sites`)` | pending |
| REQ-121 | [Plugin System & WASM Runtime](REQ-121-plugin-system.md) | `core + `crates/plugins` (new)` | pending |
| REQ-122 | [Package Install Pipeline (gates, resolver)](REQ-122-package-install-pipeline.md) | `platform` | pending |
| REQ-123 | [Feature Flags](REQ-123-feature-flags.md) | `core + admin` | pending |
| REQ-124 | [Module Manager & Selective Installation](REQ-124-module-manager.md) | `admin + core` | pending |
| REQ-125 | [Secrets & Credential Management](REQ-125-secrets-management-depth.md) | `core + infra` | pending |
| REQ-126 | [Observability Stack](REQ-126-observability-stack.md) | `infra` | pending |
| REQ-127 | [Reliability Primitives](REQ-127-reliability-primitives.md) | `core + infra` | pending |
| REQ-128 | [Deployment Tooling (Docker, Compose, Kubernetes)](REQ-128-deployment-tooling.md) | `infra + release` | pending |
| REQ-129 | [Migration Safety & Release Engineering](REQ-129-migration-safety.md) | `core + infra` | pending |
| REQ-130 | [GraphQL Surface & SDK Generation](REQ-130-graphql-and-sdk-generation.md) | ``crates/content` + `packages/api-client`` | pending |
| REQ-131 | [CLI & Generators](REQ-131-cli-and-generators.md) | ``apps/cli` + `tools/`` | pending |
| REQ-132 | [Control-Plane / Data-Plane Split](REQ-132-control-plane-split.md) | `infra + `crates/workflows`` | pending |
| REQ-133 | [Projects (Shared Automation & Credentials)](REQ-133-multi-tenancy-projects.md) | `core + admin` | pending |
| REQ-134 | [Licensing & Edition Gating](REQ-134-licensing-and-editions.md) | `platform` | pending |

> **Execution:** [`docs/BUILD-PLAN-v2.md`](../BUILD-PLAN-v2.md) groups these 64 requests into
> waves (1 = panel depth, 2 = CMS depth, 3 = automation & AI, 4 = business modules,
> 5 = platform & enterprise, 6 = deployment). The `omnion-build` loop works the queue in wave
> order, one slice per tick, with the browser QA pass as the acceptance gate.

> Note: a **mobile app** was mentioned in the brief but intentionally **not captured yet**
> (excluded by the owner for now).
