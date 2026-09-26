> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/approvals`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Approval workflows as a first-class module.

- **Approval flows**: define chains (sequential/parallel), conditions (amount > X, role = Y), escalation on timeout.
- **Requests inbox**: my pending approvals, requested-by-me, history; approve / reject / delegate with comments.
- **Attachments & context:** each request links to its subject (quote, leave, expense, page publish…).
- **Audit**: every decision recorded (who, when, comment) and surfaced in the audit log.
- **Integrations**: sales (discounts), HR (leave), accounting (expenses), CMS (publish gates).
- **Events**: `approvals.request.created`, `approvals.request.decided`.
