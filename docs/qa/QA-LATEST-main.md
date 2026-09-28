# Omnion QA — latest pass (main)

- When: 2026-09-28T06:41:27.911Z · artifacts: `qa-artifacts/20260928-064108`
- Interactions: 954 clicks · 77 field fills · 3 form submissions · 988 screenshots
- Console errors: 7 · failed requests: 6 · dialogs: 3
- Programmatic findings: 9 (high 4 · medium 5 · low 0)
- Vision issues: 0

## Top findings

- **[high] console-error** — main http://127.0.0.1:3100/media?folder=1d82c9e9-401c-43b8-9f8b-2ab51512ab55: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3100/media?folder=nonexistent-folder: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[medium] web-console** — web http://qa.omnion.test:3200/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3200/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3200/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main 404 http://127.0.0.1:3100/api/v1/media/files?site_id=6c35dc0d-ac40-4533-a7fa-86d83625ac4e&folder_id=1d82c9e9-401c-43b8-9f8b-2ab51512ab55&recursive=true&sort=newest&limit=200 
- **[high] request-failed** — main 400 http://127.0.0.1:3100/api/v1/media/files?site_id=6c35dc0d-ac40-4533-a7fa-86d83625ac4e&folder_id=nonexistent-folder&recursive=true&sort=newest&limit=200 
- **[medium] web-request** — web 404 http://qa.omnion.test:3200/ 
- **[medium] web-request** — web 404 http://qa.omnion.test:3200/ 
