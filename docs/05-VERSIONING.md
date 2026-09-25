# Omnion — Versioning & Release Management

> Captured from the owner's brief (2026-09-25, part 5). If Omnion is to be a serious platform,
> version control means more than Git: **the system itself needs layered versioning** — release
> versions, content revisions, plugin/theme compatibility, database migrations, config
> versions, feature flags.

## 1. Omnion's own release/version system

Semantic Versioning:

```text
MAJOR.MINOR.PATCH
   │     │     │
   │     │     └── Bug/security fix
   │     └──────── New feature
   └────────────── Breaking change
```

Examples:

```text
Omnion 1.4.7
Omnion 1.5.0
Omnion 2.0.0
```

And not just the Core:

```text
Omnion Core       2.4.0
CMS Module        3.1.2
CRM Plugin        1.8.0
Corporate Theme   2.2.1
```

Every component can have its own version.

## 2. Git-based development system

One repo:

```text
main
 │
 ├── develop
 │
 ├── feature/*
 ├── fix/*
 ├── refactor/*
 └── release/*
```

For example:

```text
feature/plugin-marketplace
feature/ldap-auth
fix/media-upload
refactor/theme-engine
```

PR → CI → review → merge. `main` is the stable branch that goes straight to production.

## 3. Release system

For example:

```text
feature
   ↓
develop
   ↓
release/2.4.0
   ↓
QA
   ↓
main
   ↓
TAG v2.4.0
   ↓
GitHub Release
   ↓
Docker Images
```

When the tag `v2.4.0` is created, these are produced automatically:

```text
Docker
├── omnion:2.4.0
├── omnion:2.4
└── omnion:latest
```

## 4. CMS content version control

**This part is very important.** An admin:

> changed the homepage.

and later must be able to say:

> "revert it to the previous state."

For every piece of content:

```text
Page
 │
 ├── Revision 1
 ├── Revision 2
 ├── Revision 3
 └── Revision 4 ← current
```

For example:

```text
Homepage

v1
├── Title: Welcome
└── Hero: image1.jpg

v2
├── Title: Welcome to Omnion
└── Hero: image2.jpg

v3
├── Title: Build with Omnion
└── Hero: image3.jpg
```

Admin:

```text
Revision History

v3  Current
v2  Published
v1  Draft

[Compare] [Restore]
```

## 5. A Git-like diff system

Instead of just showing "v1/v2", we should show the changes:

```diff
- Welcome to our website
+ Welcome to Omnion

- /images/hero-old.jpg
+ /images/hero-new.jpg
```

And block-level:

```text
Homepage
│
├── Hero
│   └── modified
│
├── About
│   └── unchanged
│
├── Features
│   └── added
│
└── Contact
    └── deleted
```

This makes the CMS significantly more professional.

## 6. Draft / Published versions

Content lifecycle:

```text
Draft
  ↓
Review
  ↓
Approved
  ↓
Scheduled
  ↓
Published
```

For example:

```text
Published: v12
Draft:     v13
```

While the admin works on v13, visitors still see v12. When published:

```text
v12 → archived
v13 → published
```

## 7. Scheduled publishing

For example:

```text
Version: v24
Publish at:

2026-10-01
09:00
```

A worker publishes automatically when the time comes. The same system can also be used for:

```text
Scheduled publish
Scheduled unpublish
Scheduled delete
Scheduled theme change
```

## 8. Database migration version control

For backend changes:

```text
database/
└── migrations/
    ├── 0001_initial.sql
    ├── 0002_users.sql
    ├── 0003_sites.sql
    ├── 0004_content.sql
    ├── 0005_plugins.sql
    └── 0006_revisions.sql
```

Migrations should be designed to be **reversible**. Deployment:

```text
Backup
 ↓
Migration check
 ↓
Migration
 ↓
Health check
 ↓
Application start
```

## 9. Plugin version control

Marketplace plugins also use SemVer:

```text
CRM

1.0.0
1.1.0
1.1.1
1.2.0
2.0.0
```

Manifest:

```json
{
  "id": "com.omnion.crm",
  "version": "1.4.0",
  "omnion": ">=2.3.0 <3.0.0"
}
```

When installing, Omnion checks:

```text
Plugin compatibility

CRM 1.4.0
       ↓
Omnion 2.4.0

✓ Compatible
```

## 10. Dependency/version resolver

A Composer/npm-like system. Example:

```text
CRM 2.0
 ├── Omnion >=2.4
 ├── Contacts >=1.3
 └── Forms >=3.0
```

Marketplace installer:

```text
Resolve dependencies
        ↓
Check compatibility
        ↓
Check conflicts
        ↓
Check permissions
        ↓
Install
```

If there is a conflict:

```text
Installation blocked

CRM requires:
Forms >=3.2

Installed:
Forms 2.9

Reason:
Incompatible dependency
```

## 11. Theme versioning

Themes work the same way:

```text
Corporate Theme

3.0.0
3.1.0
3.1.1
```

But more importantly: **a theme update must not break content.** Therefore:

```text
CONTENT
   │
   │ independent
   ▼
THEME
```

Switching themes:

```text
Corporate
   ↓
Minimal
```

Content stays exactly as it is.

## 12. System update manager

A dedicated screen in the admin panel:

```text
System Updates

┌─────────────────────────────────────┐
│ Omnion Core                         │
│ 2.3.1 → 2.4.0                       │
│                                     │
│ ⚠ Database migration required      │
│ ✓ Backup available                 │
│ ✓ Compatible plugins               │
│                                     │
│ [View Changes] [Update]             │
└─────────────────────────────────────┘
```

Plugins:

```text
Updates Available

CRM          1.4 → 1.5
SEO          2.1 → 2.2
Forms        3.0 → 3.1
Analytics    1.8 → 1.9
```

## 13. Update rollback

In an enterprise system, **rollback is a must**:

```text
Update
  ↓
Backup
  ↓
Migration
  ↓
Health Check
      │
      ├── ✓ OK → Continue
      │
      └── ✗ FAIL
              ↓
           Rollback
```

But there is an important distinction: **application rollback** and **database rollback** are
not the same thing. Database migrations should avoid being destructive wherever possible.

Instead of:

```text
BAD:

DROP COLUMN old_name
```

do:

```text
1. create the new column
2. move the data
3. update the application
4. drop the old column in a later release
```

This makes zero/minimal-downtime deployments much safer.

## 14. Audit version history

Who changed what?

```text
Audit Log

User: admin@company.com
Action: page.update
Resource: homepage
Revision: 42 → 43
IP: ...
Time: ...
```

Even better:

```text
Revision 43

Created by: Ahmet
Reviewed by: Mehmet
Approved by: Ayşe
Published by: Mehmet
```

which ties into the enterprise workflow.

## 15. Config versioning

Site settings can be versioned too:

```text
Site Configuration

v1
v2
v3
v4 ← current
```

So a wrong config change can be undone with:

```text
Restore configuration
```

Especially useful for critical settings such as:

- domain
- SMTP
- theme
- SEO
- integrations
- feature flags

## 16. Feature flags

Include these from the start:

```text
Feature Flags

new_editor             OFF
new_media_library      ON
experimental_search    OFF
new_dashboard          ON
```

They can differ per environment:

```text
development
staging
production
```

And even per organization/site:

```text
Organization A → ON
Organization B → OFF
```

## 17. Environment promotion

Enterprise deployment:

```text
Development
     ↓
   Staging
     ↓
   Approval
     ↓
 Production
```

The **same build artifact** is promoted. Rather than rebuilding in production, the image that
was tested in staging:

```text
omnion:2.4.0
```

is moved to production. This is a very important enterprise principle.

## 18. Repository additions

The file structure rises to this level:

```text
omnion/
│
├── apps/
├── crates/
├── packages/
├── modules/
├── plugins/
├── themes/
│
├── database/
│   ├── migrations/
│   ├── seeds/
│   └── fixtures/
│
├── versioning/
│   ├── manifests/
│   ├── compatibility/
│   ├── migrations/
│   └── changelog/
│
├── release/
│   ├── scripts/
│   ├── changelog/
│   └── artifacts/
│
├── infra/
│   ├── docker/
│   ├── compose/
│   ├── kubernetes/
│   ├── helm/
│   └── terraform/
│
├── docs/
├── tests/
├── tools/
│
├── .github/
│   └── workflows/
│
├── VERSION
├── Cargo.toml
├── package.json
├── pnpm-workspace.yaml
├── turbo.json
├── docker-compose.yml
├── Dockerfile
├── LICENSE
├── SECURITY.md
├── CONTRIBUTING.md
├── CHANGELOG.md
└── README.md
```

## Five version layers in Omnion

```text
┌──────────────────────────────────────┐
│          Omnion Release              │
│             v2.4.0                   │
├──────────────────────────────────────┤
│ Core / Modules                       │
│ v2.x                                 │
├──────────────────────────────────────┤
│ Plugins                              │
│ CRM 1.4 / SEO 2.1 / Forms 3.0        │
├──────────────────────────────────────┤
│ Themes                               │
│ Corporate 2.3 / Minimal 1.8          │
├──────────────────────────────────────┤
│ Content Revisions                    │
│ Homepage v43 / Blog v129             │
└──────────────────────────────────────┘
```

Built this way, **GitHub version control + application release management + database
migration + CMS revision history + plugin/theme compatibility** become separate but
coordinated systems — keeping the architecture manageable even when Omnion grows to hundreds
of modules and thousands of plugins.
