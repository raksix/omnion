# REQ-107 — Agent Evals & Telemetry

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Proving the agents actually work, and keep working.

- Eval suites: input sets with expected properties, run on demand and on schedule.
- Scoring (exact match, rubric, LLM-as-judge with a second model), pass rate trend.
- Regression gate: a model/prompt change must not drop the pass rate below a threshold.
- Telemetry: per-tool success rate, step counts, latency percentiles, cost per solved task.
- Panel screens for suites, runs, diffs between prompt/model versions.
