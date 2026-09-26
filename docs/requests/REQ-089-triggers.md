# REQ-089 — Triggers (webhook, schedule, polling)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows` + `crates/events`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

How workflows start.

- Webhook triggers: live and test URLs, response modes (immediate, wait-for-workflow, custom), path/method/auth config.
- Schedule trigger: cron and interval expressions with timezone and a next-runs preview.
- Polling triggers with cursor state, deduplication of seen items and backoff on failure.
- App-event triggers fed by the internal event bus (REQ-016) and the automation engine (REQ-003).
- Trigger activation registry: which triggers are armed, last fired, error state.
