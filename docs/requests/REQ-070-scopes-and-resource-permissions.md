# REQ-070 — Scopes & Resource-Level Permissions

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/permissions`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Granting exactly the right slice.

- Scope model: global, organization, site, department, module, single resource.
- Scoped role assignment UI (assign a role inside a scope, see all assignments of a user).
- Resource allow/deny lists (this page, this media folder, this site).
- Cross-site isolation guarantees with tests (no leakage between sites or organizations).
- Per-site role variation (same user, different role per site).
