# Omnion — Vision & Target Architecture

> Captured from the owner's brief (2026-09-25). Omnion is not meant to be a simple CMS:
> the target is a **corporate, multi-tenant, headless-capable platform/CMS**. Building it
> with the right architecture from the start is what makes it a solid project.
> (Repo convention: documentation in English; chat stays Turkish.)

## 1. Core / System core

Everything managed from a single admin panel:

- Multi-site creation
- Separate domain/subdomain per site
- Per-site users/roles/permissions
- Per-site settings
- Per-site theme
- Per-site language
- Per-site media
- Per-site menus
- Per-site content
- Per-site SEO settings

Example:

```text
CMS
├── Site A
│   ├── Turkish
│   ├── English
│   └── German
│
├── Site B
│   ├── Turkish
│   └── English
│
└── Site C
    └── Turkish
```

All of it is managed from **one admin panel**.

## 2. Corporate identity / LDAP

Worth designing carefully.

```text
Authentication
├── Local Account
├── LDAP
├── Active Directory
├── SAML
├── OAuth2 / OIDC
└── MFA
```

With LDAP, for example:

```text
LDAP Group
   ↓
CMS Role
   ↓
Permissions
```

Example mapping:

```text
IT-Admins
 → System Administrator

Marketing
 → Content Editor

Translation
 → Translator

HR
 → HR Module
```

So a company can connect its own AD/LDAP system.

## 3. Odoo-style module system

Arguably the most important part of the project. Instead of embedding everything into the
CMS, build a **modular architecture**.

```text
Core
 │
 ├── CMS
 ├── Users
 ├── Permissions
 ├── Sites
 ├── Localization
 ├── Media
 ├── Workflow
 │
 └── Modules
      ├── Blog
      ├── Forms
      ├── E-Commerce
      ├── CRM
      ├── HR
      ├── Documents
      ├── Events
      ├── Helpdesk
      ├── Newsletter
      └── ...
```

A module should be able to bring its own:

- database tables
- APIs
- admin pages
- permissions
- frontend components
- translations
- migrations

Later, someone should be able to write, for example:

```text
my-company-module
```

and load it into the CMS.

## 4. Theme system

Don't lock users in. Example:

```text
themes/
├── default/
├── corporate/
└── custom-theme/
```

A theme may have a structure like:

```text
theme.json
templates/
components/
assets/
styles/
scripts/
locales/
```

And users can say:

> "I will write my own Next.js/Vue/HTML theme."

Even better: a **theme API**. A theme can fetch data through APIs such as:

```js
cms.pages()
cms.posts()
cms.menu("main")
cms.site()
cms.translations()
```

That way the CMS and the frontend can be decoupled (headless capability).

## 5. Multi-language system

Avoid the classic:

```text
title_tr
title_en
title_de
```

pattern — it scales badly. Instead:

```text
Content
 ├── ID: 123
 ├── slug
 └── translations
      ├── tr
      ├── en
      ├── de
      └── fr
```

Example:

```text
Product #123

TR
title = "Kırmızı Ayakkabı"

EN
title = "Red Shoe"

DE
title = "Roter Schuh"
```

### Translation Center

A dedicated **Translation Center** in the admin panel:

```text
┌─────────────────────────────────────────────┐
│ Translation Center                          │
├─────────────────────────────────────────────┤
│ TR → EN                                     │
│                                             │
│ Kırmızı Ayakkabı                            │
│ [ Red Shoe                              ]   │
│                                             │
│ Açıklama                                    │
│ [ Description...                         ]  │
│                                             │
│             [AI Translate] [Save]           │
└─────────────────────────────────────────────┘
```

AI integration goes through a provider abstraction:

```text
Translation Engine
├── Manual
├── OpenAI
├── DeepL
├── Google
└── Custom Provider
```

so the user can pick a provider.

## 6. Translation memory

Adding this makes the project notably more professional. If:

```text
"Submit Application"
```

was translated before as:

```text
"Başvuruyu Gönder"
```

the system remembers it. Next time the same phrase appears:

> Existing translation found.

Even:

```text
Translation Memory
TM-001

Source:
Submit Application

TR:
Başvuruyu Gönder

Confidence:
100%
```

## 7. Content system

Don't stop at:

```text
Page
Post
Category
```

Build a **Content Type Builder**.

Admin:

```text
Content Types
 ├── Page
 ├── Blog Post
 ├── Product
 ├── Employee
 ├── Event
 └── Custom
```

When creating a custom type:

```text
Employee

Fields:

Name        → Text
Surname     → Text
Position    → Text
Photo       → Image
Department  → Relation
Biography   → Rich Text
```

So the user can create their own content types.

## 8. Workflow

An enterprise CMS must have it. Example:

```text
Draft
 ↓
Review
 ↓
Translation
 ↓
Approval
 ↓
Published
```

Roles:

```text
Author
Editor
Translator
Reviewer
Publisher
Administrator
```

And an audit log:

```text
User: Ahmet
Action: Updated Page
Page: About Us
Before: ...
After: ...
Date: ...
IP: ...
```

## 9. Permission system

Don't stop at:

```text
admin
editor
user
```

Build an **RBAC + permission** system. Example permissions:

```text
content.page.create
content.page.read
content.page.update
content.page.delete
content.page.publish

site.create
site.delete
site.settings.update

translation.read
translation.update

users.manage
ldap.manage
```

Roles are combinations of these permissions.

## 10. Multi-site vs multi-tenant

Important distinction — separate the two concepts from day one:

```text
Organization
      │
      ├── Site A
      ├── Site B
      └── Site C
```

For example, one company can own:

```text
Acme Corporation
│
├── acme.com
├── acme.de
├── acme.fr
└── careers.acme.com
```

under a single organization. Another customer:

```text
Company B
│
└── companyb.com
```

This architecture also makes a later SaaS transition easy.

## 11. Admin panel

Panel sketch:

```text
┌─────────────────────────────────────────────┐
│ CMS                    Site: Acme.com ▼     │
├──────────────┬──────────────────────────────┤
│ Dashboard    │                              │
│              │       Dashboard              │
│ Content      │                              │
│ Pages        │                              │
│ Posts        │                              │
│ Media        │                              │
│              │                              │
│ Sites        │                              │
│ Languages    │                              │
│ Translation  │                              │
│              │                              │
│ Modules      │                              │
│ Users        │                              │
│ Roles        │                              │
│ LDAP         │                              │
│              │                              │
│ Settings     │                              │
└──────────────┴──────────────────────────────┘
```

A **Current Site: `Acme.com`** selector at the top is very useful.

## 12. API-first

Go **API-first**. Core:

```text
REST API
GraphQL
Webhooks
Events
```

Frontend:

```text
Admin Panel
      ↓
    API
      ↓
    Core
      ↓
 Database
```

Theme:

```text
Theme
 ↓
API
 ↓
CMS
```

So later on, React / Vue / Next.js / mobile apps / any other frontend can connect easily.

## 13. Event system

Crucial for an Odoo-style modular system:

```text
PageCreated
PageUpdated
PagePublished
UserCreated
SiteCreated
TranslationUpdated
```

Modules can listen to these events. Example:

```text
PagePublished
      ↓
SEO Module
      ↓
Sitemap regenerate
      ↓
Webhook
      ↓
Search indexing
```

## 14. Overall architecture

```text
                    ┌───────────────┐
                    │ Admin Panel   │
                    └───────┬───────┘
                            │
                    ┌───────▼───────┐
                    │ API / Gateway │
                    └───────┬───────┘
                            │
        ┌───────────────────┼───────────────────┐
        │                   │                   │
   ┌────▼────┐        ┌─────▼─────┐       ┌────▼────┐
   │ CMS Core│        │ IAM/Auth   │       │ Modules │
   └────┬────┘        └─────┬─────┘       └────┬────┘
        │                   │                   │
        ├───────────┬───────┼───────────────────┤
        │           │       │                   │
     Content     Sites    LDAP              Workflow
        │
   ┌────▼─────────────┐
   │ Localization     │
   │ Translation      │
   │ Translation TM   │
   └────┬─────────────┘
        │
   ┌────▼─────┐
   │ Database │
   └──────────┘
```

### The most critical principle

Design the CMS not as a "website building program" but as an **enterprise content &
application platform**. Then all of these can sit on top of it:

```text
CMS
 ├── Website
 ├── Intranet
 ├── Corporate Portal
 ├── Documentation
 ├── E-commerce
 ├── CRM
 ├── HR
 └── Custom Business Apps
```

## Next step (after the context phase)

A full technical blueprint can be produced on request: **tech stack + folder structure +
database schema + multi-tenant model + plugin API + theme API + LDAP architecture +
translation system + Docker deployment**.
