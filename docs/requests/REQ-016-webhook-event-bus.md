# REQ-016 — Webhook + Event Bus

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/webhooks`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Every important occurrence becomes an event:

```text
page.created
page.updated
page.published
user.created
user.deleted
order.created
plugin.installed
theme.activated
```

Plugins can subscribe to these events.

## Notes

- Expands the event system sketched in docs/01-VISION.md §13.
