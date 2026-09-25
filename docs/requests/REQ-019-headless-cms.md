# REQ-019 — Headless CMS

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core API (`crates/content` + `apps/api`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Omnion must not be tied to its own Next.js renderer. Content API:

```text
/api/v1/content/pages
/api/v1/content/posts
/api/v1/media
```

So frontends can be:

- Next.js
- React
- Vue
- Nuxt
- mobile app
- custom application
