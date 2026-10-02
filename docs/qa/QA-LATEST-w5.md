# Omnion QA — latest pass (w5)

- When: 2026-10-02T00:09:47.707Z · artifacts: `qa-artifacts/20261002-000821`
- Interactions: 76 clicks · 2 field fills · 2 form submissions · 84 screenshots
- Console errors: 5 · failed requests: 4 · dialogs: 0
- Programmatic findings: 12 (high 4 · medium 8 · low 0)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

## Top findings

- **[medium] unlabeled-input** — developer-sdks: 1 input(s) without a label
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3104/: Failed to load resource: the server responded with a status of 422 (Unprocessable Entity)
- **[medium] web-console** — web http://qa.omnion.test:3204/qa-sample: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] request-failed** — main 422 http://127.0.0.1:3104/api/v1/pages?site_id=3bbaf848-90db-4761-b1ea-419fb9e88948 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/qa-sample 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] web-page** — the pass could not publish the sample page the renderer check reads (unknown reason), so the check below measures nothing
- **[high] web-page** — the published page /qa-sample did not render (status 404)
