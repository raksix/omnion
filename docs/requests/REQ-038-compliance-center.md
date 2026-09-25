# REQ-038 — Compliance Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core + admin UI
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

```text
Compliance
├── Audit
├── Data Retention
├── Data Export
├── Data Deletion
├── Access Logs
├── Security Policies
└── Privacy
```

Example operations:

> "Find all of this user's data in the system."

> "Export the user's data."

## Notes

- Data deletion must respect audit-log immutability and retention rules; ties into
  docs/05-VERSIONING.md §15 (config versioning) and docs/07-IAM.md §20 (safety invariants).
