# REQ-032 — Universal Command Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + core search
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

`Ctrl + K`:

```text
Search or command...

> Create customer
> Open CRM
> Create workflow
> Switch site
> Search invoice 4812
> Run backup
> Open analytics
> Ask AI
```

And natural language resolves to results directly:

> "Open Mehmet's last 10 tickets."

## Notes

- Extends the command palette from REQ-002; commands must respect the caller's permissions
  (docs/07-IAM.md) and audited like any other action.
