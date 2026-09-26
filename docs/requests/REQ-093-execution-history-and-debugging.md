# REQ-093 — Execution History & Debugging UI

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Finding out what a run actually did.

- Executions list: status, duration, started by, trigger, workflow version, filters and search.
- Execution detail: per-node input/output, timing, error, and the item lineage through nodes.
- Run-to-node / partial execution from the canvas ("execute step", "execute to here").
- Retry from a failed node; re-run a single execution with pinned data.
- Payload storage strategy and retention settings.
