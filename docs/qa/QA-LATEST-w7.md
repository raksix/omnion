# Omnion QA — latest pass (w7)

- When: 2026-09-28T07:43:15.732Z · artifacts: `qa-artifacts/20260928-074302`
- Interactions: 953 clicks · 79 field fills · 3 form submissions · 1004 screenshots
- Console errors: 14 · failed requests: 13 · dialogs: 2
- Programmatic findings: 23 (high 18 · medium 5 · low 0)
- Vision issues: 0

## Top findings

- **[high] console-error** — main http://127.0.0.1:3106/media?folder=89cae565-9780-4dc7-8d02-3cb38e209d03: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3106/ai: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[high] console-error** — main http://127.0.0.1:3106/ai: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3106/ai: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3106/ai: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3106/ai: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3106/ai: Failed to load resource: net::ERR_CONNECTION_REFUSED
- **[high] console-error** — main http://127.0.0.1:3106/ai: Failed to load resource: net::ERR_CONNECTION_REFUSED
- **[high] console-error** — main http://127.0.0.1:3106/media?folder=nonexistent-folder: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[medium] web-console** — web http://qa.omnion.test:3206/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3206/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3206/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main 404 http://127.0.0.1:3106/api/v1/media/files?site_id=fc0b26aa-1263-4b06-9b45-a5a7c38db7d3&folder_id=89cae565-9780-4dc7-8d02-3cb38e209d03&recursive=true&sort=newest&limit=200 
- **[high] request-failed** — main 400 http://127.0.0.1:3106/api/v1/ai/providers 
- **[high] request-failed** — main 500 http://127.0.0.1:3106/api/v1/ai/providers 
- **[high] request-failed** — main 500 http://127.0.0.1:3106/api/v1/ai/providers 
- **[high] request-failed** — main 500 http://127.0.0.1:3106/api/v1/ai/models 
- **[high] request-failed** — main 500 http://127.0.0.1:3106/api/v1/ai/models 
- **[high] request-failed** — main net http://127.0.0.1:3106/api/v1/ai/providers net::ERR_CONNECTION_REFUSED
- **[high] request-failed** — main net http://127.0.0.1:3106/api/v1/ai/providers net::ERR_CONNECTION_REFUSED
- **[high] request-failed** — main 400 http://127.0.0.1:3106/api/v1/media/files?site_id=fc0b26aa-1263-4b06-9b45-a5a7c38db7d3&folder_id=nonexistent-folder&recursive=true&sort=newest&limit=200 
- **[medium] web-request** — web 404 http://qa.omnion.test:3206/ 
- **[medium] web-request** — web 404 http://qa.omnion.test:3206/ 
