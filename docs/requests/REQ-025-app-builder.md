# REQ-025 — App Builder / No-Code Builder

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform (`apps/admin` + Studio)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Users can create their own application without writing code:

```text
New App
 ↓
Choose Data
 ↓
Create Fields
 ↓
Create Views
 ↓
Create Workflow
 ↓
Create Permissions
 ↓
Publish
```

Example — a company builds an internal **Asset Tracking** app:

```text
Asset
├── Name
├── Serial Number
├── Employee
├── Department
├── Purchase Date
└── Status
```

Omnion then generates automatically:

- CRUD
- API
- admin panel
- permissions
- workflow
- audit
- search

## Notes

- A very large feature; built on the dynamic data model (REQ-026) and surfaced in
  Omnion Studio (REQ-049); AI-driven variant: REQ-045.
