# REQ-042 — AI Copilots in Every Module

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** AI Hub + per-module UI
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Instead of a single AI screen — a copilot in each module:

```text
CRM        → CRM Copilot
Accounting → Finance Copilot
CMS        → Content Copilot
HR         → HR Copilot
Support    → Support Copilot
```

All backed by the same AI Hub.

## Notes

- Copilots are thin UI shells over AI Hub agents (docs/06-AI-HUB.md §14–15); each copilot
  ships with a preset role and tool permissions (docs/07-IAM.md §14).
