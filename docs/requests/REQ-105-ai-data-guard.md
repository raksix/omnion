# REQ-105 — AI Data Guard (PII protection)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Keeping sensitive data out of prompts.

- PII detection on outbound prompt content (names, e-mails, phones, IDs, IBAN-like patterns).
- Masking with placeholder mapping and response re-mapping so results stay coherent.
- Rules per provider/feature: block, mask, or allow; per-organization policy.
- Audit of blocked/masked events; test harness with sample payloads.
- Documentation of the residual risk (masking is defensive, not a guarantee).
