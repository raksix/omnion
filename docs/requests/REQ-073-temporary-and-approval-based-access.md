# REQ-073 — Temporary & Approval-Based Access

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/permissions`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Time-boxed and approved elevation.

- Temporary roles with automatic expiry (duration picker, countdown, auto-revoke).
- Emergency Administrator elevation (logged, notified, expiring by default).
- Access request workflow: request → approver → time-boxed grant → record.
- Integration with the approvals module (REQ-059) and the audit log.
- "Who has elevated access right now" view.
