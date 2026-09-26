# REQ-096 — Queue Mode & Scaling

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Running automation at volume.

- Worker pools with concurrency control per workflow and per queue.
- Queue naming and routing; job processor; worker health endpoint.
- Leader election and distributed locking so scheduled work runs once.
- Pub/sub fan-out of execution events; cross-instance execution stop.
- Multi-instance webhook relay and worker lifecycle events pushed to the control plane.
