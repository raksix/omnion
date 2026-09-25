# REQ-028 — BI / Reporting Engine

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/reporting`)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Ask in natural language:

> "Show me the last 6 months of sales by department."

AI translates it into a query:

```text
Natural Language
      ↓
AI
      ↓
Query Builder
      ↓
PostgreSQL
      ↓
Chart
```

A report builder also ships with:

- Pivot
- Grouping
- Filters
- Charts
- Export
- Scheduled reports

## Notes

- Natural-language querying is an AI Hub consumer (docs/06-AI-HUB.md §11 pattern); all
  queries must run through permission scoping (docs/07-IAM.md).
