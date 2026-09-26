# REQ-123 — Feature Flags

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Turning things on safely.

- Flag registry: key, description, default, rollout state, owner.
- Scopes: environment, organization, site, user segment.
- Panel screen to toggle flags with an audit trail and "who changed this" history.
- Flags readable in backend code, admin UI and theme/rendered output.
- Kill-switch semantics documented (rollout, percentage, schedule).
