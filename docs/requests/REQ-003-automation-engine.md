# REQ-003 — Automation Engine

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core engine (`crates/workflows`) + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A Zapier/Make-style automation engine:

```text
TRIGGER
   ↓
CONDITION
   ↓
ACTION
```

Example:

```text
New user created
        ↓
Role = Customer
        ↓
Send welcome email
        ↓
Create CRM record
        ↓
Call webhook
```

Users build these visually in the UI with the workflow builder (REQ-004).

## Notes

- Engine research: [`docs/09-N8N-TEARDOWN.md`](../09-N8N-TEARDOWN.md) — deep source-level
  teardown of the n8n engine v2.41 (durable steps, queues, waits, HITL approvals) with
  adopt/avoid lessons.
