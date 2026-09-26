# REQ-129 — Migration Safety & Release Engineering

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + infra
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Schema changes that never lose a row.

- Numbered, reversible SQL migrations with `up`/`down` verified in CI (up → down → up on a fixture DB).
- Non-destructive patterns enforced by policy (add-nullable → backfill → constrain; never drop-then-add in one step).
- Deployment sequence: migrations run before the new code serves traffic; app and DB rollback documented separately.
- Seed and fixture data for local/demo installs; anonymised dump tooling for support.
- Zero/minimal-downtime strategy: additive migrations, dual reads, backfill jobs.
