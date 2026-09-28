# Omnion QA — latest pass (w2)

- When: 2026-09-27T21:55:01.841Z · artifacts: `../../../dev/shm/omnion-qa-w2/20260927-215439`
- Interactions: 943 clicks · 74 field fills · 3 form submissions · 968 screenshots
- Console errors: 10 · failed requests: 8 · dialogs: 1
- Programmatic findings: 15 (high 10 · medium 5 · low 0)
- Vision issues: 2

## Top findings

- **[high] console-error** — main http://127.0.0.1:3101/ai: Failed to load resource: the server responded with a status of 409 (Conflict)
- **[high] console-error** — main http://127.0.0.1:3201/qa-block-page?site=main: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3201/qa-block-page?site=main: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3101/: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3101/: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[medium] web-console** — web http://qa.omnion.test:3201/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3201/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3201/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main 409 http://127.0.0.1:3101/api/v1/ai/chat 
- **[high] request-failed** — main 404 http://127.0.0.1:3201/qa-block-page?site=main 
- **[high] request-failed** — main 500 http://127.0.0.1:3101/api/v1/onboarding 
- **[high] request-failed** — main 500 http://127.0.0.1:3101/api/v1/sites 
- **[medium] web-request** — web 404 http://qa.omnion.test:3201/ 
- **[medium] web-request** — web 404 http://qa.omnion.test:3201/ 
- **[high] click-error** — [ai] "Send" (button) → console-error:  error: Failed to load resource: the server responded with a status of 409 (Conflict)
