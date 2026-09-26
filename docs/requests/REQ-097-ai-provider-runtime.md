# REQ-097 — AI Provider Runtime & Local Models

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Bring-your-own models, cloud or local.

- Provider registry with typed capability flags: chat, stream, embeddings, image generation, audio, transcription, list-models.
- Cloud providers (OpenAI-compatible, Anthropic-style, Google, and any OpenAI-compatible endpoint) plus local inference (Ollama, vLLM, llama.cpp server).
- Per-provider API key (write-only), base URL, custom model list, enable/disable, connection test.
- Provider protocol adapters (chat completions, responses, messages) with streaming normalisation.
- Health/usage telemetry per provider and automatic failover when a provider errors.
