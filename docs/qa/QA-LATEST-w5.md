# Omnion QA — latest pass (w5)

- When: 2026-10-01T18:02:41.894Z · artifacts: `../../../dev/shm/qa-w5-artifacts/20261001-174226`
- Interactions: 5 clicks · 2 field fills · 0 form submissions · 30 screenshots
- Console errors: 16 · failed requests: 13 · dialogs: 0
- Programmatic findings: 32 (high 27 · medium 3 · low 2)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

## Top findings

- **[high] console-error** — main http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[high] console-error** — main http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[high] console-error** — main http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[high] console-error** — main http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[high] console-error** — main http://127.0.0.1:3104/developer/logs: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3104/developer/logs: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3104/developer/api-explorer: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3104/developer/api-explorer: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — mobile http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[high] console-error** — mobile http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[high] console-error** — mobile http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[high] console-error** — mobile http://127.0.0.1:3104/login: Error: Internal Next.js error: Router action dispatched before initialization.
- **[high] console-error** — mobile http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[high] console-error** — mobile http://127.0.0.1:3104/login: Failed to load resource: the server responded with a status of 503 (Service Unavailable)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Error: the content API answered 503 for home
- **[high] request-failed** — main 503 http://127.0.0.1:3104/api/v1/onboarding 
- **[high] request-failed** — main 503 http://127.0.0.1:3104/api/v1/onboarding 
- **[high] request-failed** — main 503 http://127.0.0.1:3104/api/v1/onboarding 
- **[high] request-failed** — main 400 http://127.0.0.1:3104/api/v1/auth/login 
- **[high] request-failed** — main 500 http://127.0.0.1:3104/developer/logs 
- **[high] request-failed** — main 500 http://127.0.0.1:3104/developer/api-explorer 
- **[high] request-failed** — main 500 http://127.0.0.1:3104/developer/api-explorer 
- **[high] request-failed** — mobile 503 http://127.0.0.1:3104/api/v1/onboarding 
- **[high] request-failed** — mobile 503 http://127.0.0.1:3104/api/v1/auth/login 
