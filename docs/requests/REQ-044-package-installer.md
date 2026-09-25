# REQ-044 — Package / App Installer

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform (module manager)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

A company says:

> "Install CRM + Helpdesk + Marketing + AI Agent for me."

Omnion:

```text
Resolve Dependencies
       ↓
Install Modules
       ↓
Create Permissions
       ↓
Create Roles
       ↓
Run Migrations
       ↓
Enable Workflows
       ↓
Ready
```

## Notes

- The dependency resolution semantics were specified in docs/05-VERSIONING.md §10; this is
  the one-click UX on top. Bigger sibling: App Marketplace (REQ-048).
