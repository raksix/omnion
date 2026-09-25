# REQ-024 — Deployment Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + infra
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

From the admin panel:

```text
Deployment

Production
● Healthy

Version
2.4.1

Available
2.5.0

[View Changes]
[Deploy]
[Rollback]
```

For Kubernetes-based enterprise deployments, also show:

```text
Replicas: 6
CPU: ...
Memory: ...
```

## Notes

- Ties into the update manager / rollback flow in docs/05-VERSIONING.md (§12–§13).
