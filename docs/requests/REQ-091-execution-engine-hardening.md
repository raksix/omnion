# REQ-091 — Execution Engine Hardening

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Making long automation runs trustworthy.

- Durable step store: every step's result is persisted before the next begins.
- Queue-driven step workers with an orchestration worker and batch steps.
- Loop ledger preventing double execution; endless-loop guard; run filters to skip nodes.
- Per-node retry policy with resumed-error exclusion; continueOnFail with per-item error capture.
- Cancellation that reaches running steps; error-workflow routing; wait sweeper (60s cadence, batched).
