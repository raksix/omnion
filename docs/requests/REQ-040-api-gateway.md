# REQ-040 — API Gateway

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`apps/api` edge)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Expose to the outside world:

```text
/api/v1
/api/v2
```

while managing:

- API keys
- OAuth
- rate limit
- quotas
- analytics
- versioning
- scopes
- IP restrictions

## Notes

- Complements the versioned-API design in docs/02-ARCHITECTURE.md and the service-account
  identities in docs/07-IAM.md §15.
