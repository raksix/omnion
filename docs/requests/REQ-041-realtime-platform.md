# REQ-041 — Real-time Platform

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (WS/SSE layer) + admin
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

WebSocket / SSE:

```text
Admin A
   │
   ├── Page updated
   │
   └──────────→ Admin B
```

Notifications, workflow executions, AI streaming, logs — all update live.

## Notes

- n8n's push/event patterns (docs/09 §6 lifecycle events) are prior art; keep one event bus
  for UI, webhooks (REQ-016) and automations.
