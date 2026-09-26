# REQ-128 — Deployment Tooling (Docker, Compose, Kubernetes)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra + release
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Shipping it anywhere.

- Root Dockerfile per app (api, admin, web, cli) with multi-stage builds and small runtime images.
- Compose files: developer stack (exists), small-company production stack, enterprise topology (separate DB/queue/storage).
- Helm chart with values for ingress, TLS, resources, secrets, autoscaling.
- Release pipeline producing versioned images, CLI binaries and the chart on tag.
- Upgrade documentation covering database migrations and rolling restarts.
