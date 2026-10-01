# Omnion QA — latest pass (w4)

- When: 2026-10-01T17:42:46.128Z · artifacts: `qa-artifacts/20261001-173751`
- Interactions: 1518 clicks · 181 field fills · 81 form submissions · 1762 screenshots
- Console errors: 311 · failed requests: 303 · dialogs: 2
- Programmatic findings: 738 (high 714 · medium 17 · low 7)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

> **THIS RUN IS NOT A VERDICT — 1 reason(s):** 6 screenshot(s) could not be written.
> The counts below were computed without the evidence a verdict needs. Re-run the pass.

## Top findings

- **[high] broken-image** — media-settings: http://127.0.0.1:3103/api/v1/media/5576d3cc-e373-4ab3-89b0-6041b488731a/raw?preset=standard
- **[medium] low-contrast** — sales-quotes: 1 text node(s) under WCAG AA, e.g. {"text":"0","ratio":1,"min":4.5,"fontSize":12}
- **[medium] low-contrast** — sales-orders: 1 text node(s) under WCAG AA, e.g. {"text":"0","ratio":1,"min":4.5,"fontSize":12}
- **[medium] low-contrast** — inventory-stock: 2 text node(s) under WCAG AA, e.g. {"text":"Any","ratio":1.05,"min":4.5,"fontSize":12}
- **[medium] low-contrast** — inventory-approvals: 1 text node(s) under WCAG AA, e.g. {"text":"Waiting","ratio":1.05,"min":4.5,"fontSize":12.5}
- **[medium] low-contrast** — accounting-invoices: 1 text node(s) under WCAG AA, e.g. {"text":"All","ratio":2.64,"min":4.5,"fontSize":12.5}
- **[medium] unlabeled-input** — accounting-invoice-new: 5 input(s) without a label
- **[medium] low-contrast** — accounting-payments: 1 text node(s) under WCAG AA, e.g. {"text":"All","ratio":2.64,"min":4.5,"fontSize":12.5}
- **[high] unmeasured-page** — hr-attendance-roster: the page walk failed before it produced diagnostics (Error: page.evaluate: Execution context was destroyed, most likely because of a navigation) — this screen was not measured
- **[medium] low-contrast** — security-events: 1 text node(s) under WCAG AA, e.g. {"text":"Apply","ratio":1.19,"min":4.5,"fontSize":12.5}
- **[medium] offscreen-mobile** — mobile crm-contacts: 4 element(s) outside the viewport
- **[medium] offscreen-mobile** — mobile crm-companies: 1 element(s) outside the viewport
- **[high] crm-state** — state step failed: contacts_hasAState
- **[high] crm-state** — state step failed: contacts_readsAsASentence
- **[high] crm-state** — state step failed: contacts_showsTheRequestId
- **[high] crm-state** — state step failed: contacts_hasARetry
- **[high] crm-state** — state step failed: companies_hasAState
- **[high] crm-state** — state step failed: companies_readsAsASentence
- **[high] crm-state** — state step failed: companies_showsTheRequestId
- **[high] crm-state** — state step failed: companies_hasARetry
- **[high] crm-state** — state step failed: deals_hasAState
- **[high] crm-state** — state step failed: deals_readsAsASentence
- **[high] crm-state** — state step failed: deals_showsTheRequestId
- **[high] crm-state** — state step failed: deals_hasARetry
- **[high] crm-state** — state step failed: theScreenWasBrokenBeforeTheRetry
