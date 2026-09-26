# REQ-098 — Model Registry & Router

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Picking the right model for the job.

- Model catalog with metadata: context window, tool support, vision, streaming, embedding, cost per 1K tokens.
- Capability flags surfaced in the panel (searchable, filterable table).
- Task-based routing: cheap, translation, coding, vision, long-context, embedding, critical — each mapping to a preferred model with fallbacks.
- Per-organization/site default model and per-feature override (e.g. copilots vs content generation).
- Route explanation in AI logs: which model answered a request and why.
