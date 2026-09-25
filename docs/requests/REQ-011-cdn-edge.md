# REQ-011 — CDN / Edge System

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform / infra
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

```text
User
 ↓
CDN
 ↓
Edge Cache
 ↓
Omnion
```

Cache invalidation:

```text
Page published
     ↓
Purge CDN cache
     ↓
New version live
```
