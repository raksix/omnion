# REQ-126 — Observability Stack

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Seeing the system from outside.

- Structured JSON logging with request ids and user/org attribution.
- Prometheus metrics (HTTP, DB, queue, workflow, AI usage) and a /metrics endpoint.
- OpenTelemetry tracing across HTTP, DB, queue and AI calls.
- Grafana dashboard bundle shipped in infra/ with alerts for the usual suspects.
- Health/readiness/liveness endpoints + graceful shutdown, wired into the deployment tooling.
