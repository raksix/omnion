# Omnion QA — latest pass (w5)

- When: 2026-10-01T23:47:10.786Z · artifacts: `qa-artifacts/20261001-234640`
- Interactions: 76 clicks · 2 field fills · 2 form submissions · 118 screenshots
- Console errors: 9 · failed requests: 8 · dialogs: 0
- Programmatic findings: 25 (high 17 · medium 8 · low 0)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

## Top findings

- **[high] depth-pass-failed** — the "devEvents" depth pass did not complete: Error: 2 dev-events claim(s) did not hold: theSubscribeLinkCarriesTheName, theSampleCoversEveryRequiredField
- **[high] depth-pass-failed** — the "devSdks" depth pass did not complete: Error: 4 dev-sdks claim(s) did not hold: thePreviewHidesTheDotfilesItAdmitsToHiding, theLookupShowsWhoIsAsking, theScopesAreSpokenNotSpelled, approvalIsConfirmed
- **[high] unknown-pass-name** — --only=oauth-contract matches no route and no depth pass
- **[high] unknown-pass-name** — --only=dev-sdk-screen matches no route and no depth pass
- **[high] unknown-pass-name** — --only=dev-sdk-tab-fallback matches no route and no depth pass
- **[medium] unlabeled-input** — developer-sdks: 1 input(s) without a label
- **[high] console-error** — main http://127.0.0.1:3104/developer/oauth-apps: Failed to load resource: the server responded with a status of 500 (Internal Server Error)
- **[high] console-error** — main http://127.0.0.1:3104/developer/oauth-apps: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[high] console-error** — main http://127.0.0.1:3104/developer/sdks?tab=cli: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[high] console-error** — main http://127.0.0.1:3104/developer/sdks?tab=cli: Failed to load resource: the server responded with a status of 400 (Bad Request)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] console-error** — main http://127.0.0.1:3104/: Failed to load resource: the server responded with a status of 422 (Unprocessable Entity)
- **[medium] web-console** — web http://qa.omnion.test:3204/qa-sample: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[medium] web-console** — web http://qa.omnion.test:3204/: Failed to load resource: the server responded with a status of 404 (Not Found)
- **[high] request-failed** — main 500 http://127.0.0.1:3104/api/v1/oauth-apps/068656f2-d6a2-45d7-9f6c-e5635e72ead2/rotate 
- **[high] request-failed** — main 400 http://127.0.0.1:3104/api/v1/oauth-apps/068656f2-d6a2-45d7-9f6c-e5635e72ead2 
- **[high] request-failed** — main 400 http://127.0.0.1:3104/api/v1/dev/cli/device-code/SPW5-SXDH 
- **[high] request-failed** — main 400 http://127.0.0.1:3104/api/v1/dev/cli/device-code/SPW5-SXDH 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] request-failed** — main 422 http://127.0.0.1:3104/api/v1/pages?site_id=8ed96864-6b8c-436e-ac36-53d28db653c1 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/qa-sample 
- **[medium] web-request** — web 404 http://qa.omnion.test:3204/ 
- **[high] web-page** — the pass could not publish the sample page the renderer check reads (unknown reason), so the check below measures nothing
- **[high] web-page** — the published page /qa-sample did not render (status 404)
