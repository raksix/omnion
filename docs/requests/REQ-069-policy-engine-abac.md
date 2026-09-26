# REQ-069 — Policy Engine (ABAC)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/policy-engine`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Attribute-based rules on top of roles.

- Policy store with named policies; WHEN/AND/THEN builder in the panel (no code required).
- Conditions on user attributes (department, title, employment type), resource attributes (owner, site, status, amount) and numeric comparisons.
- AND chaining, negative conditions, time windows.
- Decision pipeline: role allow → policy deny → policy allow, with a recorded reason.
- Safety: policy validation, dry-run against a user/resource pair, policy audit trail.
