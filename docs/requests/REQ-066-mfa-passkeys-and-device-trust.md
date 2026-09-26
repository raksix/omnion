# REQ-066 — MFA, Passkeys & Device Trust

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Account protection beyond passwords.

- TOTP MFA (enrol, verify, recovery codes), optional WebAuthn/passkey login.
- Login policies (require MFA for roles, allowed login hours, IP allow/deny lists).
- Password policies (length, complexity, rotation hint, breached-password check hook).
- Session management screen: active sessions with device, IP, last seen, revoke.
- Device management: trusted devices, revoke trust, "remember this device" window.
- Step-up authentication for dangerous operations (deploy, delete organization, key rotation).
