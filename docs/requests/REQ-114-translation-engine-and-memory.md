# REQ-114 — Translation Engine & Translation Memory

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/translation`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Multilingual content without re-translating the world.

- Provider abstraction for machine translation (OpenAI, DeepL, Google, custom) with per-organization configuration.
- Manual translation engine as a first-class provider (human edits count as translations).
- Translation memory: reuse of previously approved strings with confidence scores.
- "Existing translation" notice when a source string changed after its translation.
- Bulk translate a page/collection into N languages; per-field target; progress and error reporting.
- Glossary per site (do-not-translate terms, preferred terms).
