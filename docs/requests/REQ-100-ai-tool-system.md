# REQ-100 — AI Tool System & Permission Matrix

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + `crates/permissions`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Every action an AI may take is a named, permissioned tool.

- Content tools: search, read, create, update, publish, rollback.
- Media tools: search, upload; user tools: search, create; site/theme/plugin tools: get, update, list, install, activate.
- Ops tools: workflow.start, analytics.query, deployment.preview, deployment.deploy, deployment.read, deployment.restart, logs.read, health.read, seo.analyze.
- Each tool declares the permission it needs; the AI acts as an identity with grants and explicit denies.
- Tool registry screen in the panel: which tools are enabled, for which agents, with usage counts.
