# REQ-125 — Secrets & Credential Management

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Credentials that never sit in a text file.

- Encryption at rest for all stored credentials (per-installation key), with rotation support.
- Credential entities: API keys, OAuth tokens, SMTP accounts, payment keys, SSH keys.
- External secret providers (Vault-style, file-based, env-based) with instance-level credential assignment.
- Deployment keys for CI and remote environments; helper-mediated access so plaintext is never returned to the browser.
- Access audit: who read which secret, when, and from where.
