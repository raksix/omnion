# REQ-084 — Theme SDK & Packaging

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `packages/theme-sdk` + CLI
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Third parties must be able to build themes.

- `omnion.theme.json` manifest (name, version, engine range, slots, settings schema, preview).
- Directory contract: layouts/, pages/, components/, blocks/, assets/, styles/, locales/.
- Theme API: site(), page(), menu(), posts(), media(), translations(), settings() — typed helpers.
- `omnion theme create` scaffolding CLI and `omnion theme validate` (CI-able).
- Package format for distribution (zip), versioning rules, compatibility matrix, changelog.
