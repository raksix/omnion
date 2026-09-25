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
| REQ-001 | [AI Engine](REQ-001-ai-engine.md) | module (`modules/ai`) | pending |
| REQ-002 | [Global Search Engine](REQ-002-global-search.md) | core (`crates/search`) + admin | pending |
| REQ-003 | [Automation Engine](REQ-003-automation-engine.md) | core (`crates/workflows`) + admin | pending |
| REQ-004 | [Visual Workflow Builder](REQ-004-visual-workflow-builder.md) | `apps/admin` + `crates/workflows` | pending |
| REQ-005 | [Organization / Tenant System](REQ-005-organization-tenant-system.md) | core (`crates/identity`) | pending |
| REQ-006 | [Advanced IAM](REQ-006-advanced-iam.md) | core (`crates/auth`, `crates/permissions`) | pending |
| REQ-007 | [Analytics](REQ-007-analytics.md) | module (`modules/analytics`) | pending |
| REQ-008 | [Commerce Engine](REQ-008-commerce-engine.md) | module (`modules/ecommerce`) | pending |
| REQ-009 | [Helpdesk / Ticket System](REQ-009-helpdesk-tickets.md) | module (`modules/helpdesk`) | pending |
| REQ-010 | [Enterprise File Manager](REQ-010-enterprise-file-manager.md) | core (`crates/media`) + admin | pending |
| REQ-011 | [CDN / Edge System](REQ-011-cdn-edge.md) | platform / infra | pending |
| REQ-012 | [Security Center](REQ-012-security-center.md) | core + admin | pending |
| REQ-013 | [Backup Center](REQ-013-backup-center.md) | core + admin | pending |
| REQ-014 | [System Health](REQ-014-system-health.md) | core + admin | pending |
| REQ-015 | [Integration Hub](REQ-015-integration-hub.md) | module (`modules/integrations`) | pending |
| REQ-016 | [Webhook + Event Bus](REQ-016-webhook-event-bus.md) | core (`crates/webhooks`) | pending |
| REQ-017 | [Sandbox / Staging](REQ-017-sandbox-staging.md) | platform | pending |
| REQ-018 | [Preview System](REQ-018-preview-system.md) | `apps/web` + core | pending |
| REQ-019 | [Headless CMS](REQ-019-headless-cms.md) | core API (`crates/content` + `apps/api`) | pending |
| REQ-020 | [Globalization](REQ-020-globalization.md) | core (`crates/localization`) | pending |
| REQ-021 | [Notification Center](REQ-021-notification-center.md) | core (`crates/notifications`) + admin | pending |
| REQ-022 | [Developer Portal](REQ-022-developer-portal.md) | `apps/admin` + core | pending |
| REQ-023 | [Marketplace Expansion](REQ-023-marketplace-expansion.md) | `apps/marketplace` | pending |
| REQ-024 | [Deployment Center](REQ-024-deployment-center.md) | `apps/admin` + infra | pending |
| REQ-025 | [App Builder / No-Code Builder](REQ-025-app-builder.md) | platform (`apps/admin` + Studio) | pending |
| REQ-026 | [Dynamic Data Model](REQ-026-dynamic-data-model.md) | core (dynamic entity layer) | pending |
| REQ-027 | [Dashboard Builder](REQ-027-dashboard-builder.md) | `apps/admin` (Studio) | pending |
| REQ-028 | [BI / Reporting Engine](REQ-028-bi-reporting.md) | module (`modules/reporting`) | pending |
| REQ-029 | [PDF / Document Generation](REQ-029-pdf-document-generation.md) | core service (documents) | pending |
| REQ-030 | [E-signature](REQ-030-e-signature.md) | module (`modules/esignature`) | pending |
| REQ-031 | [Import / Export Center](REQ-031-import-export-center.md) | core + admin UI | pending |
| REQ-032 | [Universal Command Center](REQ-032-command-center.md) | `apps/admin` + core search | pending |
| REQ-033 | [Internal Developer Platform](REQ-033-internal-developer-platform.md) | `apps/admin` + SDKs | pending |
| REQ-034 | [Sandbox Environments](REQ-034-sandbox-environments.md) | platform | pending |
| REQ-035 | [Edge / Multi-region](REQ-035-edge-multi-region.md) | platform / infra | pending |
| REQ-036 | [Offline / Air-gapped Mode](REQ-036-air-gapped-mode.md) | platform / infra | pending |
| REQ-037 | [Secrets Manager](REQ-037-secrets-manager.md) | core + integrations | pending |
| REQ-038 | [Compliance Center](REQ-038-compliance-center.md) | core + admin UI | pending |
| REQ-039 | [Advanced Audit](REQ-039-advanced-audit.md) | core (`crates/audit`) | pending |
| REQ-040 | [API Gateway](REQ-040-api-gateway.md) | core (`apps/api` edge) | pending |
| REQ-041 | [Real-time Platform](REQ-041-realtime-platform.md) | core (WS/SSE) + admin | pending |
| REQ-042 | [AI Copilots](REQ-042-ai-copilots.md) | AI Hub + per-module UI | pending |
| REQ-043 | [White-label](REQ-043-white-label.md) | platform | pending |
| REQ-044 | [Package / App Installer](REQ-044-package-installer.md) | platform (module manager) | pending |
| REQ-045 | [AI App Builder](REQ-045-ai-app-builder.md) *(headline)* | AI Hub × App Builder | pending |
| REQ-046 | [AI Workflow Builder](REQ-046-ai-workflow-builder.md) *(headline)* | AI Hub × workflow engine | pending |
| REQ-047 | [AI Admin](REQ-047-ai-admin.md) *(headline)* | AI Hub × audit/logs | pending |
| REQ-048 | [App Marketplace](REQ-048-app-marketplace.md) *(headline)* | `apps/marketplace` | pending |
| REQ-049 | [Omnion Studio](REQ-049-omnion-studio.md) *(headline)* | new `apps/studio` | pending |
| REQ-050 | [First-run Onboarding / Setup Wizard](REQ-050-first-run-onboarding.md) | platform (`apps/admin` + CLI) | pending |

> Note: a **mobile app** was mentioned in the brief but intentionally **not captured yet**
> (excluded by the owner for now).
