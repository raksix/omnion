# REQ-074 — Permission Simulator & Authorization Diagnostics

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + core (`crates/authorization`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Answering "why can/can't they?" without guessing.

- Simulator inputs: user, scope, resource, action; run a check.
- Allow trace: which role/group/policy granted it, and at what scope.
- Deny reason: the explicit deny or missing grant, shown in plain language.
- Diagnostics panel also used by support to explain production behaviour.
- Regression safety: simulator snapshots for critical permission combinations (tests).
