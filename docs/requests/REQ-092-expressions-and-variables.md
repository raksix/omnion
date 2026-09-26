# REQ-092 — Expressions, Data Mapping & Variables

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Moving data between nodes safely.

- Expression language with sandboxing (AST allow-list, resource limits, violation errors).
- Data mapping UI: drag a field from upstream data into a parameter.
- Pinned data for development runs; mock data per node.
- Instance and project variables (used in expressions) with per-environment values.
- Environments: production/staging value sets for variables and credentials.
