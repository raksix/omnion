# Omnion QA — latest pass (w5)

- When: 2026-09-27T20:11:41.350Z · artifacts: `qa-artifacts/20260927-201127`
- Interactions: 890 clicks · 76 field fills · 1 form submissions · 926 screenshots
- Console errors: 14 · failed requests: 13 · dialogs: 2
- Programmatic findings: 24 (high 19 · medium 5 · low 0)
- Vision issues: 0

## Top findings

- **[high] console-error** — main http://127.0.0.1:3104/settings/iam/security: Failed to load resource: net::ERR_INSUFFICIENT_RESOURCES
- **[high] console-error** — main http://127.0.0.1:3104/settings/iam/security: Failed to load resource: net::ERR_INSUFFICIENT_RESOURCES
- **[high] console-error** — main http://127.0.0.1:3104/settings/iam/security: Failed to load resource: net::ERR_INSUFFICIENT_RESOURCES
- **[high] console-error** — main http://127.0.0.1:3104/settings/iam/security: Failed to load resource: net::ERR_INSUFFICIENT_RESOURCES
- **[high] console-error** — main http://127.0.0.1:3104/settings/iam/security: Failed to load resource: net::ERR_INSUFFICIENT_RESOURCES
- **[high] console-error** — main http://127.0.0.1:3104/settings/iam/security: Failed to load resource: net::ERR_INSUFFICIENT_RESOURCES
- **[high] console-error** — main http://127.0.0.1:3104/organizations: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[high] console-error** — main http://127.0.0.1:3104/organizations/57ca475d-90fa-4193-ba01-c1791881aa3a: Failed to load resource: the server responded with a status of 409 (Conflict)
- **[high] console-error** — main http://127.0.0.1:3104/organizations/57ca475d-90fa-4193-ba01-c1791881aa3a: Failed to load resource: the server responded with a status of 409 (Conflict)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main net http://127.0.0.1:3104/icon.svg?icon.0lm94lj5tsy3b.svg net::ERR_INSUFFICIENT_RESOURCES
- **[high] request-failed** — main net http://127.0.0.1:3104/api/v1/me/organizations net::ERR_INSUFFICIENT_RESOURCES
- **[high] request-failed** — main net http://127.0.0.1:3104/api/v1/sites net::ERR_INSUFFICIENT_RESOURCES
- **[high] request-failed** — main net http://127.0.0.1:3104/api/v1/organizations net::ERR_INSUFFICIENT_RESOURCES
- **[high] request-failed** — main net http://127.0.0.1:3104/api/v1/me/organizations net::ERR_INSUFFICIENT_RESOURCES
- **[high] request-failed** — main net http://127.0.0.1:3104/api/v1/organizations net::ERR_INSUFFICIENT_RESOURCES
- **[high] request-failed** — main 400 http://127.0.0.1:3104/api/v1/organizations 
- **[high] request-failed** — main 409 http://127.0.0.1:3104/api/v1/organizations/57ca475d-90fa-4193-ba01-c1791881aa3a/invitations 
- **[high] request-failed** — main 409 http://127.0.0.1:3104/api/v1/organizations/57ca475d-90fa-4193-ba01-c1791881aa3a/invitations 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] click-error** — [organizations] "Create organization" (button) → console-error:  error: Failed to load resource: the server responded with a status of 400 (Bad Request)
