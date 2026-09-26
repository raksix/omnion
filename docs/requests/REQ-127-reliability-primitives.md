# REQ-127 — Reliability Primitives

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The boring guarantees that keep a platform honest.

- Idempotency keys for mutating endpoints and job submissions.
- Retry policies with exponential backoff and jitter per subsystem (webhooks, e-mail, AI, workflows).
- Rate limiting per user/organization/IP with configurable budgets and 429 semantics.
- Circuit breaking for outbound providers with half-open probing.
- Request sanitisation, HMAC verification for inbound webhooks, and payload size limits.
