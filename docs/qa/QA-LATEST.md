# Omnion QA — latest pass

- When: 2026-09-26T10:19:51.987Z · artifacts: `qa-artifacts/20260926-101942`
- Interactions: 40 clicks · 5 field fills · 0 form submissions · 47 screenshots
- Console errors: 5 · failed requests: 2 · dialogs: 0
- Programmatic findings: 13 (high 3 · medium 10 · low 0)
- Vision issues: 3

## Programmatic findings (high + medium)

- **[medium] low-contrast** — overview: 2 text node(s) under WCAG AA, e.g. {"text":"O","ratio":3.9,"min":4.5,"fontSize":14}
- **[medium] low-contrast** — pages: 1 text node(s) under WCAG AA, e.g. {"text":"O","ratio":3.9,"min":4.5,"fontSize":14}
- **[medium] low-contrast** — media: 1 text node(s) under WCAG AA, e.g. {"text":"O","ratio":3.9,"min":4.5,"fontSize":14}
- **[medium] low-contrast** — sites: 3 text node(s) under WCAG AA, e.g. {"text":"O","ratio":3.9,"min":4.5,"fontSize":14}
- **[medium] low-contrast** — ai: 1 text node(s) under WCAG AA, e.g. {"text":"O","ratio":3.9,"min":4.5,"fontSize":14}
- **[high] console-error** — main http://127.0.0.1:3100/ai: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[medium] web-console** — web http://qa.omnion.test:3200/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3200/: WebSocket connection to 'ws://qa.omnion.test:3200/_next/hmr?id=lf6UVUAjmzTRH1SHLoRVT' failed: Error during WebSocket handshake: net::ERR_INVALID_HTTP_RESPONSE
- **[medium] web-console** — web http://qa.omnion.test:3200/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3200/: WebSocket connection to 'ws://qa.omnion.test:3200/_next/hmr?id=lf6UVUAjmzTRH1SHLoRVT' failed: Error during WebSocket handshake: net::ERR_INVALID_HTTP_RESPONSE
- **[high] request-failed** — main 400 http://127.0.0.1:3100/api/v1/ai/providers 
- **[medium] web-request** — web 404 http://qa.omnion.test:3200/ 
- **[high] click-error** — [ai] "Connect" (button) → console-error: error: Failed to load resource: the server responded with a status of 400 (Bad Request)

## Vision review

- **[low] mobile-ai** — bottom-left floating button: A dark circular button with a white 'N' is visible at the bottom-left corner, which is the Next.js development indicator and not part of the admin panel UI.
- **[high] web-home** — entire page / viewport: The screenshot is completely blank white with no visible content — no header, sidebar, navigation, cards, text, or any UI elements rendered at all.
- **[medium] click-ai-1-navigated** — bottom-left sidebar footer: A black circular 'N' badge overlaps and partially obscures the 'Sign out' text, visually cutting off the label.
