# REQ-034 — Sandbox Environments

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform (org-scoped environments)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Every organization can create a sandbox:

```text
Production
    │
    └── Create Sandbox
              ↓
          Sandbox Copy
```

There they can test:

- plugins
- themes
- workflows
- AI agents
- database changes

Then:

**Promote to Production**

## Notes

- Extends the staging flow of REQ-017; promotion uses the same-artifact principle from
  docs/05-VERSIONING.md §17.
