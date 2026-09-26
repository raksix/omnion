# REQ-106 — Local & Air-gapped AI Mode

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Running with no external calls at all.

- Local inference endpoints as first-class providers (Ollama, vLLM, llama.cpp, any OpenAI-compatible local server).
- Model management: list local models, pull/remove, show resource usage.
- Air-gapped mode switch that refuses any non-local provider call (with a clear error).
- Embedding and reranking models served locally; offline knowledge base operation.
- Install/run documentation and a doctor check that verifies local availability.
