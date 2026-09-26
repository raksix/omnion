# REQ-079 — AI Command Center (cross-module jobs)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + ai
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

One sentence → a multi-step, approved business action.

- Natural-language job input ("find overdue invoices, find the account owners, create tasks, draft the e-mails").
- Plan preview: each step shown with the tools it will use and the data it will touch.
- Approval gate before execution; per-step approve/reject; edit a step before running.
- Running-job summary UI (steps, status, tokens, cost) and job history.
- Reuses the AI Hub tools + audit; every step is permission-checked as the requesting user.
