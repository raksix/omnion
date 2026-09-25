# Omnion — Feature Requests (inbox)

> The platform feature pool, captured as **individual requests** from the owner's brief
> (2026-09-25). One request per feature layer; each gets implemented (or re-scoped) later —
> they are capture, not commitment yet.
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

> Note: a **mobile app** was mentioned in the brief but intentionally **not captured yet**
> (excluded by the owner for now).
