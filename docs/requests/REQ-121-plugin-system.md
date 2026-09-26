# REQ-121 — Plugin System & WASM Runtime

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + `crates/plugins` (new)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Extending the platform without forking it.

- Plugin manifest (name, version, engine range, permissions, hooks, migrations, settings schema).
- Plugin backend: registered routes, background jobs, event subscriptions, hooks (before/after content save, publish, user create…).
- Plugin frontend: admin screens, blocks, widgets, settings panels through the module SDK.
- Plugin-declared permissions (REQ-068) and locales; migrations run with the platform's runner.
- WASM sandbox for untrusted plugin logic with resource limits; lifecycle (install/enable/disable/uninstall) with data retention choices.
