# REQ-072 — Service Accounts & API Keys

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Non-human identities.

- Service account registry (name, purpose, owner, last used, status) with their own grants and explicit denies.
- API keys per service account: create, scope, rotate, revoke, expiry, usage counter.
- Documented service identities: CI/CD, analytics, backup worker, AI agent, external CRM.
- Key authentication path (header), audit of every key-authenticated request.
- Panel screens for keys, rotation reminders, and last-used visibility.
