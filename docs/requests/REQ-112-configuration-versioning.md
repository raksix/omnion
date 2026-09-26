# REQ-112 — Configuration Versioning & Restore

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Treating settings like content.

- Versioned critical settings (site settings, theme settings, feature flags, integrations, automation rules).
- Every change appends a version with author, diff and reason.
- Restore a previous configuration version; compare two versions.
- Export/import a configuration bundle (per site or whole installation).
- Audit trail and optional approval gate for high-risk settings.
