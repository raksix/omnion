# Omnion — Technical Architecture & Stack Direction

> Captured from the owner's brief (2026-09-25, part 2). Omnion is designed not as a "CMS project"
> but as a **corporate, open-source platform that contains a CMS**. Target:
>
> - A small company runs it with **a single Docker Compose**.
> - A big company deploys it to a **Kubernetes cluster**.
> - Use just the CMS, or add modules like CRM/HR/workflow.

## Base architecture

```text
                         OMNION
                            │
             ┌──────────────┴──────────────┐
             │                             │
       Admin Console                  Public API
       Next.js/React                  REST/OpenAPI
             │                             │
             └──────────────┬──────────────┘
                            │
                     ┌──────▼──────┐
                     │ Omnion Core │
                     │    Rust     │
                     │    Axum     │
                     └──────┬──────┘
                            │
       ┌────────────┬───────┼────────┬────────────┐
       ▼            ▼       ▼        ▼            ▼
    PostgreSQL    Redis    S3      Workers      Search
```

## Modular monolith

Do not build 30 microservices on day one. The core is a single application, modular inside:

```text
omnion-core/
├── identity/
├── users/
├── organizations/
├── sites/
├── domains/
├── content/
├── media/
├── localization/
├── translation/
├── themes/
├── workflows/
├── permissions/
├── audit/
├── notifications/
├── webhooks/
├── search/
└── modules/
```

If real scale demands it later, we split specific parts out into services.

## Enterprise side

**Identity**

- Local authentication
- LDAP
- Active Directory
- OIDC
- SAML
- MFA
- WebAuthn

**Authorization**

- RBAC
- ABAC/policy
- Permissions scoped by organization / site / module / resource / action

Example:

```text
company-a
 ├── Site A
 │    ├── Editor
 │    └── Translator
 │
 └── Site B
      └── Marketing
```

A user can have access to Site A while having none to Site B.

## CMS side

Must be genuinely powerful:

```text
CMS
├── Pages
├── Posts
├── Custom Content Types
├── Media
├── Menus
├── Forms
├── Taxonomies
├── Revisions
├── Drafts
├── Publishing
├── Scheduling
├── SEO
└── Redirects
```

### Content Builder

Users can create their own content types, for example:

```text
Employee
├── Name
├── Position
├── Department
├── Photo
└── Biography
```

## Multi-site

A single Omnion installation:

```text
Omnion
│
├── company.com
├── company.de
├── company.fr
├── careers.company.com
└── docs.company.com
```

all managed **from one panel**. Each site keeps its own scope for:

- domain
- theme
- language
- content
- users
- menus
- settings
- SEO

## Docker

Design this especially well.

### Development

```text
docker compose up -d
```

brings up:

```text
omnion
postgres
redis
minio
mailpit
```

### Production — small company

```text
Docker
│
├── omnion
├── postgres
├── redis
└── minio
```

### Enterprise

```text
                    Load Balancer
                         │
              ┌──────────┼──────────┐
              ▼          ▼          ▼
          Omnion-1   Omnion-2   Omnion-3
              │          │          │
              └──────────┼──────────┘
                         │
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
     PostgreSQL        Redis          S3
       Cluster
```

And an official Helm chart for Kubernetes:

```text
Helm Chart
│
├── deployment
├── service
├── ingress
├── config
├── secrets
├── HPA
└── jobs
```

## Reliability

For an enterprise product, design these in **from the first architecture**:

```text
Health checks
Readiness probes
Liveness probes
Graceful shutdown
Database migrations
Backup / restore
Retry policies
Idempotency
Rate limiting
Circuit breaking
Audit logs
Structured logging
Metrics
Distributed tracing
```

Observability:

```text
Omnion
 ├── OpenTelemetry
 ├── Prometheus
 ├── Grafana
 └── structured logs
```

So when the company's sysadmin asks "where is it slow?", they can actually see the answer.

## Plugin system

This can set Omnion apart from an ordinary CMS:

```text
Omnion
│
├── Core
│
└── Plugins
    ├── CRM
    ├── HR
    ├── E-Commerce
    ├── Helpdesk
    ├── Events
    └── Custom
```

A plugin ships:

```text
plugin/
├── manifest
├── permissions
├── migrations
├── backend
├── frontend
├── locales
└── hooks
```

For execution, evaluating a **WASM sandbox** makes a lot of sense — it keeps third-party
plugins from getting unrestricted access to the Core.

## Theme system

Similarly:

```text
Theme
├── templates
├── components
├── assets
├── locales
├── config
└── theme manifest
```

When a user says "I'll write my own corporate theme", they can do it without cracking the system.

## Translation

Multi-language is one of the core features:

```text
Localization
├── Languages
├── Locale
├── Translation Keys
├── Translation Memory
├── Translation Workflow
└── Translation Providers
```

AI translation goes through a provider abstraction:

```text
Omnion
   │
Translation API
   │
├── Manual
├── OpenAI
├── DeepL
├── Google
└── Custom
```

## API

Design the API **versioned** from the start:

```text
/api/v1/...
```

OpenAPI:

```text
OpenAPI Specification
        ↓
SDK generation
        ↓
TypeScript
Python
Go
Rust
```

Webhooks can emit events such as:

```text
page.published
user.created
site.created
translation.updated
order.created
```

## The most important decision

Position Omnion as:

> **Omnion is an open-source enterprise application platform with a powerful CMS at its core.**

The CMS is just one part of it:

```text
                    OMNION
                       │
       ┌───────────────┼────────────────┐
       │               │                │
      CMS             IAM            Platform
       │               │                │
   Websites          LDAP             API
   Content           SSO             Events
   Media             RBAC            Plugins
   Themes            MFA             Webhooks
   Translation                       Workflow
       │
       └───────────────┬────────────────┘
                       │
                 Business Modules
              CRM / HR / ERP / ...
```

**Owner's preferred technical main line:**

> **Rust + Axum + Tokio + SQLx + PostgreSQL + Redis + S3/MinIO + Next.js + React + TypeScript
> + Docker + Kubernetes + OpenTelemetry + WASM plugin runtime**

This fits both the self-hosted open-source world and the deployment model of large
organizations.

And the right first move is to start the project as a solid **modular monolith** instead of
splitting it into microservices from the start. We extract a component into a service not when
user numbers grow, but **when a specific component genuinely needs to scale independently**.
