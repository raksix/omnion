# REQ-130 — GraphQL Surface & SDK Generation

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/content` + `packages/api-client`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Integration beyond REST.

- GraphQL endpoint over the content/tenancy/media surface with persisted queries and depth limits.
- Schema per enablement: only types the caller may read; field-level permission filtering.
- OpenAPI document generated from the REST routers; SDKs generated for TypeScript (and one more language) in CI.
- Versioned API policy: deprecation headers, changelog, sunset windows.
- Playground/explorer for authenticated admins (REQ-022 developer portal).
