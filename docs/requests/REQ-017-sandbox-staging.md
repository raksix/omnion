# REQ-017 — Sandbox / Staging

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

The admin clicks **Create Staging Environment**:

```text
Production
     │
     └── Clone
          ↓
       Staging
```

Changes are tried there first:

```text
Staging
 ↓
Preview
 ↓
Approve
 ↓
Deploy to Production
```
