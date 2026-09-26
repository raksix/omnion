# REQ-101 — AI Approvals & Action Preview

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Nothing dangerous happens without a human.

- Approval gates by tool class: publish, delete, plugin install, theme change, deployment, database operation.
- Action preview: the exact field-level diff an AI operation would produce, before it runs.
- Conversation-to-operations: the AI proposes a change set; the user edits and confirms it.
- Dangerous-action warnings with a typed confirmation for irreversible steps.
- Every decision recorded in the audit log with the requester, model and diff.
