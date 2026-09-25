# REQ-004 — Visual Workflow Builder

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + `crates/workflows`
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A proper node editor:

```text
[User Created]
      ↓
[Check Role]
   ↙      ↘
Admin    Customer
 ↓          ↓
Email      CRM
```

Plugins can extend it with new node types.

## Notes

- Builder UX prior art + engine research: [`docs/09-N8N-TEARDOWN.md`](../09-N8N-TEARDOWN.md)
  (§8: test webhooks / "listen for test event", waiting/resume, HITL signed callbacks).
