# REQ-119 — Notification Channels & Templates

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/notifications`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Reaching people where they are.

- Channels: in-app, e-mail (SMTP), webhook, and a pluggable channel interface (SMS/chat later).
- Template editor per notification type with variables, preview and test send.
- Per-user preferences: which categories, which channels, quiet hours; digest options (daily/weekly).
- Delivery log with retries and failure reasons; unsubscribe handling for outbound e-mail.
- Events from every module can raise notifications without code changes.
