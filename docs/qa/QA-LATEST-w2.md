# Omnion QA — latest pass (w2)

- When: 2026-09-27T17:30:19.982Z · artifacts: `qa-artifacts/20260927-173006`
- Interactions: 910 clicks · 74 field fills · 1 form submissions · 935 screenshots
- Console errors: 8 · failed requests: 5 · dialogs: 2
- Programmatic findings: 9 (high 4 · medium 5 · low 0)
- Vision issues: 0

## Top findings

- **[high] console-error** — main http://127.0.0.1:3101/settings/iam/security: A tree hydrated but some attributes of the server rendered HTML didn't match the client properties. This won't be patched up. This can happen if a SSR-ed Client Component used:

- 
- **[high] console-error** — main http://127.0.0.1:3201/qa-block-page: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3201/qa-block-page: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3201/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3201/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3201/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main 404 http://127.0.0.1:3201/qa-block-page 
- **[medium] web-request** — web 404 http://qa.omnion.test:3201/ 
- **[medium] web-request** — web 404 http://qa.omnion.test:3201/ 
