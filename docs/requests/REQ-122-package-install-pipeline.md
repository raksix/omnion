# REQ-122 — Package Install Pipeline (gates, resolver)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Installing things safely.

- Install pipeline with gates: signature/hash check, engine compatibility, permission review, migration dry-run.
- Dependency and version resolver with a compatibility matrix; conflict blocking that explains the reason.
- Atomic install with rollback on failure; uninstall with data-removal choice.
- Update path for installed packages (patch/minor/major rules) and a changelog view.
- Package registry metadata for the marketplace (REQ-048) and the installer CLI (REQ-131).
