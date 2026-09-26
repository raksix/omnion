# REQ-131 — CLI & Generators

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `apps/cli` + `tools/`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Everything a developer needs from the terminal.

- `omnion dev|build|test|migrate|seed|doctor` — no hand-run Docker commands for the common path.
- `omnion plugin create` / `omnion theme create` scaffolds with CI-ready tests and docs stubs.
- `omnion doctor` verifies environment, database, redis, storage, migrations and AI providers.
- `omnion backup create|restore|verify` wrapping the backup centre (REQ-013).
- Machine-readable output (`--json`) for scripts and CI.
