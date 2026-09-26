# REQ-099 — Agent Runtime & Tool Loop

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Agents that actually do things.

- Agent loop with streaming sinks, tool-call execution, step counter and stop conditions.
- Agent state and conversation persistence; resumable runs; workspace per agent.
- Skills registry (validated skill definitions) and skills runtime.
- Guardrails: untrusted-content handling, tool allow-lists, output verification helper.
- Telemetry per run (steps, tokens, cost, tools used) and an SDK for embedding the runtime.
