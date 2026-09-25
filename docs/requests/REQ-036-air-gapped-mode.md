# REQ-036 — Offline / Air-gapped Mode

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform / infra
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Especially for enterprises — the system runs without any internet access:

```text
Internet
   X

Omnion
 ├── Local LLM
 ├── Local Storage
 ├── Local Registry
 └── Local Marketplace Mirror
```

## Notes

- Builds on the local-AI support defined in docs/06-AI-HUB.md §19 (Ollama/vLLM).
