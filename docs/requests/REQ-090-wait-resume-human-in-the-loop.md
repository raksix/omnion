# REQ-090 — Wait, Resume & Human-in-the-Loop

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Long-running workflows that pause for people.

- Wait node (duration or until a date) backed by the wait sweeper.
- Waiting webhooks and waiting forms: workflow pauses, a URL is issued, resume on submission.
- Send-and-wait (message + approval) and approvals as resumable webhooks.
- HMAC-signed callbacks so a resume request cannot be forged.
- Pending-response handling, timeout branches, and a "waiting" view listing paused runs.
