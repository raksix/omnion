# REQ-132 — Control-Plane / Data-Plane Split

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Scaling the engine separately from the panel.

- Standalone engine service (workflows, automation, webhooks) with its own process and health.
- Control-plane server pushing lifecycle events; batched event relay to the panel.
- Response channel back to the caller for synchronous webhook responses.
- Task-runner isolation (in-process sandbox vs child process per risky task).
- Deployment recipes for split vs single-process topologies.
