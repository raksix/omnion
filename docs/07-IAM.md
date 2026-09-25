# Omnion — Identity & Access Management (IAM)

> Captured from the owner's brief (2026-09-25, part 7). Built as an enterprise RBAC/ABAC
> system — more advanced than Discord's role model: users are not limited to preset roles,
> **they can create as many roles as they want and combine permissions freely.**

## Base structure

```text
Organization
│
├── Users
├── Groups
├── Roles
├── Permissions
├── Teams
└── Policies
```

## 1. Fully custom roles

Admin:

```text
Settings
└── Roles
    ├── Super Admin
    ├── Editor
    ├── SEO Manager
    ├── Support
    └── + Create Role
```

New role:

```text
Create Role

Name:
Marketing Manager

Description:
Content permissions for the marketing team

Color:
●

Icon:
[icon]
```

Then the permissions are selected.

## 2. Permission system

Instead of coarse `admin/editor` levels, permissions are granular.

Content:

```text
content.pages.read
content.pages.create
content.pages.update
content.pages.delete
content.pages.publish
content.pages.schedule
content.pages.restore
```

Media:

```text
media.read
media.upload
media.update
media.delete
media.manage
```

Users:

```text
users.read
users.create
users.update
users.delete
users.impersonate
```

Plugins:

```text
plugins.read
plugins.install
plugins.update
plugins.disable
plugins.uninstall
```

Deployment:

```text
deployment.read
deployment.preview
deployment.deploy
deployment.rollback
```

Tenancy (P04):

```text
organizations.read
organizations.manage
sites.read
sites.create
sites.update
sites.delete
domains.manage
```

## 3. Discord-style role hierarchy

```text
Owner
 ↓
Administrator
 ↓
Manager
 ↓
Moderator
 ↓
Editor
 ↓
Member
```

Each role has a **position/priority** value:

```text
Owner       1000
Admin        900
Manager      700
Moderator    500
Editor       300
Member       100
```

So rules like this are possible:

> A Moderator cannot change the roles of someone with the Admin role.

## 4. Role inheritance

```text
Editor
  ↓ inherits
Content Manager
  ↓ inherits
Marketing Manager
```

For example:

```text
Editor
├── pages.read
├── pages.create
└── pages.update

Marketing Manager
├── inherits Editor
├── pages.publish
├── media.manage
└── analytics.read
```

Roles don't have to be rebuilt from scratch.

## 5. Allow / Deny

Explicit deny exists alongside allows:

```text
Marketing Manager

Pages
✓ Read
✓ Create
✓ Update
✓ Publish
✗ Delete
```

Here `✗ Delete` is an explicit **deny**. Permissions inherited from a parent role — e.g.
`pages.delete` — can be overridden in a child role. The security precedence is defined
clearly:

```text
Explicit Deny
      ↓
Explicit Allow
      ↓
Inherited Allow
      ↓
Default Deny
```

## 6. Scope system

One of the points where Omnion leaves plain Discord behind: a role is not just "present/absent"
— it can be assigned at these levels:

```text
Global
Organization
Site
Department
Module
Resource
```

Example:

```text
Ahmet
└── Marketing Manager
      └── Site: company.com
```

Ahmet:

```text
company.com
✓ can manage

blog.company.com
✗ no access
```

## 7. Organization → Site → Team

Example:

```text
Acme Corporation
│
├── Site: acme.com
│
├── Site: shop.acme.com
│
├── Marketing Team
│
├── IT Team
│
└── Support Team
```

User:

```text
Ahmet
├── Organization Role: Employee
├── Team: Marketing
└── Site Role:
      acme.com → Editor
```

The same user elsewhere:

```text
shop.acme.com → Viewer
```

## 8. Groups / Teams

Assigning a role to every person in a 1000-person company is absurd. So:

```text
Groups
├── Marketing
├── Engineering
├── HR
├── Support
└── Management
```

A role can be assigned to a group:

```text
Marketing
   ↓
Marketing Manager
```

Everyone added to the group gets the permissions automatically.

## 9. Multiple roles per user

The more advanced form of the Discord model:

```text
Ahmet

Roles:
Administrator
Developer
SEO Manager
Site Editor
```

Permissions merge:

```text
Administrator
      +
Developer
      +
SEO Manager
      ↓
Effective Permissions
```

The admin panel can show this:

```text
Effective Permissions

✓ pages.read
✓ pages.update
✓ pages.publish
✓ plugins.install
✓ analytics.read
...
```

## 10. Resource-level permissions

**Extremely important.** A user may hold `pages.update` without being allowed to edit every
page:

```text
Marketing Manager

Site:
acme.com

Can edit:
├── /blog/*
├── /campaigns/*
└── /landing-pages/*

Cannot edit:
├── /legal/*
└── /hr/*
```

The trifecta: **Permission + Scope + Resource.**

## 11. ABAC

Alongside RBAC, attribute-based access control:

```text
Allow page.publish
IF
user.department == "marketing"
AND
page.site == user.site
AND
page.status == "approved"
```

Another example:

```text
Allow invoice.read

IF:
user.department == finance
AND
invoice.amount < 10000
```

So enterprise customers can write highly advanced policies.

## 12. Policy Builder

Not everyone should have to write code for this. UI:

```text
Create Policy

WHEN
    User Department
    [Marketing]

AND

    Site
    [acme.com]

AND

    Content Type
    [Blog Post]

THEN

    Allow
    [Publish]
```

Save as:

```text
Marketing Blog Publisher
```

## 13. The Owner role is special

The top of the system:

```text
Owner
```

is not fully controlled by the normal permission system. `Organization Owner` can:

- delete the organization
- change the owner
- billing
- security policy
- admin management
- all roles
- all API keys

But it still produces **audit log** entries.

## 14. The same role system applies to AI

Directly connected to the AI Hub:

```text
AI Agent
   ↓
Role
   ↓
Permissions
   ↓
Policy
   ↓
Tool
```

Example:

```text
SEO AI

Role:
SEO Assistant

Permissions:
✓ content.read
✓ content.update
✓ seo.analyze

✗ content.delete
✗ content.publish
✗ users.manage
```

The AI behaves like any other user/agent identity. A very powerful architecture.

## 15. API Key / Service Account

Systems need identities too, not just people:

```text
Service Accounts

CI/CD
Analytics Bot
Backup Worker
AI Agent
External CRM
```

Example:

```text
GitHub Actions

Permissions:
✓ deployment.read
✓ deployment.deploy
✗ users.read
✗ content.delete
```

It connects via an API key.

## 16. Temporary Roles

Very useful in enterprise. Example:

```text
Emergency Administrator

Valid:
2026-09-25 22:00
      ↓
2026-09-26 02:00
```

Automatically removed when the time is up. Same pattern for:

```text
Temporary Editor
Temporary Support
Temporary Deployment Access
```

## 17. Approval-based permission

Some operations are request-based:

```text
User requests:
deployment.deploy
```

Admin:

```text
[Approve] [Reject]
```

Once approved:

```text
Permission active for 2 hours
```

## 18. Permission Simulator

Should absolutely exist in the admin panel:

```text
Permission Simulator

User:
Ahmet

Resource:
Homepage

Action:
Publish
```

Result:

```text
✓ ALLOWED

Source:
Role → Marketing Manager
Permission → content.pages.publish
Scope → Site: acme.com
```

or:

```text
✗ DENIED

Reason:
Explicit deny from role "Content Reviewer"
```

This solves real problems in complex enterprise permission setups.

## 19. Role audit history

The role itself is versioned:

```text
Marketing Manager

v1
pages.read
pages.update

v2
+ pages.publish

v3
- pages.delete
```

Audit:

```text
22:31 Mehmet added pages.publish
22:35 Ayşe changed role priority
22:40 Mehmet removed pages.delete
```

## 20. Permission Safety

An admin must not be able to accidentally lock themselves out:

```text
You are removing your last organization-owner permission.

This action is blocked.
```

Plus invariants such as:

```text
At least one Owner must exist.
At least one Administrator must exist.
```

## The final Identity architecture

```text
                         IDENTITY
                            │
          ┌─────────────────┼─────────────────┐
          │                 │                 │
        Users             Groups          Service Accounts
          │                 │                 │
          └────────────┬────┴─────────────────┘
                       │
                     Roles
                       │
                ┌──────┴──────┐
                │             │
          Inheritance       Priority
                │             │
                └──────┬──────┘
                       │
                  Permissions
                       │
                ┌──────┴──────┐
                │             │
              RBAC           ABAC
                │             │
                └──────┬──────┘
                       │
                    Policies
                       │
                    Scope
                       │
            ┌──────────┼──────────┐
            │          │          │
       Organization   Site      Resource
                       │
                       ▼
                  Authorization
                       │
              ┌────────┴────────┐
              │                 │
           Allowed             Denied
```

Built this way, roles in Omnion are genuinely user-creatable (like Discord) — while behind the
scenes it becomes a far more corporate **IAM + RBAC + ABAC + policy engine**.

And rather than squeezing this into `crates/permissions`, the cleaner split is:

```text
crates/
├── identity/
├── permissions/
├── policy-engine/
├── authorization/
└── audit/
```

The AI Hub, the plugin system, the marketplace, the CMS and the deployment system all use the
same authorization infrastructure.
