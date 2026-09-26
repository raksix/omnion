# REQ-134 — Licensing & Edition Gating

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Open-core business model, enforced in code.

- Edition model: community (open) vs enterprise (licensed) with a signed licence file.
- Feature gating declared in one place (a capability registry) and enforced in the API, not just the UI.
- Licence screen: installed licence, expiry, seats, entitled features, renewal action.
- Grace behaviour for expired licences (read-only vs hard stop, configurable).
- Never break the open-source core: gating must never remove existing open functionality.
