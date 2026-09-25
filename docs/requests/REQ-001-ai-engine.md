# REQ-001 — AI Engine

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/ai`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Give the platform a dedicated **Omnion AI Engine**:

- AI provider abstraction
- OpenAI / Anthropic / Gemini / local LLM support
- Per-site AI configuration
- Content generation
- Content summarization
- Automatic SEO
- Alt-text generation
- Translation
- Document analysis
- AI chatbot
- RAG / knowledge base
- Embedding / vector search
- AI workflow actions (see REQ-003)
- AI agent/plugin system
- Natural-language operations inside the admin

## Example

> "Analyze the posts published in the last 30 days and list the ones with weak SEO."

## Notes

- Lives under `modules/` — the Core stays thin.
- Reuses the provider-abstraction pattern planned for translation (docs/01-VISION.md, §5).
- Natural-language admin operations build on the command palette (REQ-002) and the
  automation engine (REQ-003).
