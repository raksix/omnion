# Omnion QA — latest pass (w5)

- When: 2026-10-01T17:33:28.054Z · artifacts: `../../../dev/shm/qa-w5-artifacts/20261001-173127`
- Interactions: 112 clicks · 3 field fills · 3 form submissions · 126 screenshots
- Console errors: 7 · failed requests: 6 · dialogs: 0
- Programmatic findings: 15 (high 8 · medium 7 · low 0)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

## Top findings

- **[high] console-error** — main http://127.0.0.1:3104/developer/api-explorer: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[high] console-error** — main http://127.0.0.1:3104/developer/api-explorer: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3104/: Failed to load resource: the server responded with a status of 422 (Unprocessable Entity)
- **[medium] web-console** — web http://qa.omnion.test:3204/qa-sample: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main 400 http://127.0.0.1:3104/api/v1/dev/explorer/requests 
- **[high] request-failed** — main 400 http://127.0.0.1:3104/api/v1/dev/explorer/requests 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] request-failed** — main 422 http://127.0.0.1:3104/api/v1/pages?site_id=7e5afb93-b7fe-42b6-bc3a-3979c8a92e4a 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/qa-sample 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] web-page** — the pass could not publish the sample page the renderer check reads (unknown reason), so the check below measures nothing
- **[high] web-page** — the published page /qa-sample did not render (status 404)
