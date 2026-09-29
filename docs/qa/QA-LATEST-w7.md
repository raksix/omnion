# Omnion QA — latest pass (w7)

- When: 2026-09-29T19:27:42.538Z · artifacts: `qa-artifacts/20260929-182501`
- Interactions: 1724 clicks · 103 field fills · 52 form submissions · 1677 screenshots
- Console errors: 22048 · failed requests: 22040 · dialogs: 0
- Programmatic findings: 43567 (high 43560 · medium 7 · low 0)
- Vision issues: 4

## Top findings

- **[high] ai-state** — the routing screen rendered 0 task rows instead of all seven
- **[high] ai-state** — saving a routing chain was refused on the panel: this action requires the "ai.providers.read" permission
- **[high] ai-state** — a saved routing chain rendered no candidate, so the save did not take effect
- **[high] ai-state** — the dry run rendered no walk, which is the one thing the routing screen exists for
- **[high] ai-state** — the decision log is not empty but rendered no rows and no empty state
- **[high] ai-state** — the decision log rendered its error banner on a healthy stack
- **[high] ai-state** — the decision log export was refused: this action requires the "ai.usage.read" permission
- **[high] ai-state** — the decision log export reported nothing, so an operator cannot tell it worked
- **[high] ai-state** — ai-states: one failing provider list also blanked the model registry — they are not independent
- **[high] ai-state** — ai-states: the retry button left the error on screen after the endpoint recovered
- **[high] ai-state** — ai-states: one failing model registry also blanked the provider list — they are not independent
- **[high] console-error** — main http://127.0.0.1:3106/setup: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[high] console-error** — main http://127.0.0.1:3106/login: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/login: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/login: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/login: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3106/login: Failed to load resource: the server responded with a status of 403 (Forbidden)
