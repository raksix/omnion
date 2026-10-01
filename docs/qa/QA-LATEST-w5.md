# Omnion QA — latest pass (w5)

- When: 2026-10-01T02:07:42.241Z · artifacts: `../../../dev/shm/qa-w5-tick86e/20261001-010708`
- Interactions: 109 clicks · 8 field fills · 3 form submissions · 119 screenshots
- Console errors: 5 · failed requests: 4 · dialogs: 0
- Programmatic findings: 14 (high 6 · medium 7 · low 1)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

## Top findings

- **[high] console-error** — main http://127.0.0.1:3104/cdn/rules: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/qa-sample: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main 500 http://127.0.0.1:3104/api/v1/cdn/rules?site_id=29744210-de6b-41ba-bf40-ec6d854dc713 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/qa-sample 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] click-error** — [cdn-settings] "Sessions" (a) → click-error: locator.click: Timeout 4500ms exceeded.
Call log:
  - waiting for locator('[data-qa-idx="26"]')
    - locator resolved to <a data-qa-idx="26" href="/settings/iam/sessions" class="flex items-cente
- **[high] click-error** — [cdn-settings] "Devices" (a) → click-error: locator.click: Timeout 4500ms exceeded.
Call log:
  - waiting for locator('[data-qa-idx="27"]')
    - locator resolved to <a data-qa-idx="27" href="/settings/iam/devices" class="flex items-center 
- **[high] click-error** — [cdn-settings] "Search settings" (a) → click-error: locator.click: Timeout 4500ms exceeded.
Call log:
  - waiting for locator('[data-qa-idx="28"]')
    - locator resolved to <a data-qa-idx="28" href="/settings/search" class="flex items-cent
- **[high] web-page** — the published page /qa-sample did not render (status 404)
