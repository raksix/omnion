# REQ-068 — Permission Catalogue & Effective Permissions

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/permissions`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The permission vocabulary and the "why can this user do that" answer.

- Catalogue screen: every permission key (content.pages.publish, media.upload, deployment.deploy …) grouped by module with description and bound endpoints.
- Explicit deny that overrides every inherited allow; default deny; precedence resolution order documented in the UI.
- Effective permissions viewer per user: resolved list with the source (role, group, scope, service account).
- Multiple roles per user merged correctly; conflict highlighting.
- Scope-aware catalogue: global, organization, site, department, module and resource scopes.
