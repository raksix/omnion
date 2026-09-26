# REQ-080 — Admin Activity Timeline

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + core (`crates/audit`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

What happened in this installation, in human language.

- Timeline screen: merges audit entries, deploys, AI jobs, automation runs and login events.
- Filters: actor, module, severity, date range; free-text search.
- Row detail with metadata, related resource links and (for AI) the model/prompt reference.
- Live updates while work is running; export to CSV.
- Powers the "who changed this?" affordance everywhere else in the panel.
