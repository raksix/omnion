# REQ-081 — Frontend Packages & UI Kit

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `packages/*`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The shared frontend foundation every app consumes.

- `@omnion/ui`: button, input, modal, dropdown, tabs, card, table, dialog, toast, empty state, skeleton.
- Component override mechanism so a theme or module can restyle without forking.
- `@omnion/types` (contracts), `@omnion/api-client` (typed fetchers), `@omnion/theme-sdk`.
- Frontend module SDK: a module can ship admin screens, blocks and settings panels.
- Editor UI package: node canvas graph layout, CodeMirror 6 integration, expression editor.
- Frontend insights + OpenTelemetry module (client error + performance reporting).
