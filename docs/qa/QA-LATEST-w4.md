# Omnion QA — latest pass (w4)

- When: 2026-09-30T20:43:31.499Z · artifacts: `qa-artifacts/20260930-191521`
- Interactions: 145 clicks · 8 field fills · 3 form submissions · 119 screenshots
- Console errors: 26 · failed requests: 13 · dialogs: 0
- Programmatic findings: 76 (high 76 · medium 0 · low 0)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

## Top findings

- **[high] unknown-pass-name** — --only=crm matches no route and no depth pass
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
- **[high] crm-state** — state step failed: theRetryRecoversTheScreen
- **[high] crm-state** — state step failed: theBoardReplacesItsBodyOnARefusal
- **[high] crm-state** — state step failed: theBoardNamesTheRequest
- **[high] crm-state** — state step failed: theOtherViewIsStillOffered
- **[high] console-error** — main http://127.0.0.1:3103/crm/deals: Error: useCrmList must be used inside <CrmShell>
- **[high] console-error** — main http://127.0.0.1:3103/crm/leads: Failed to load resource: the server responded with a status of 403 (Forbidden)
- **[high] console-error** — main http://127.0.0.1:3103/crm/companies: Failed to load resource: the server responded with a status of 409 (Conflict)
- **[high] console-error** — main http://127.0.0.1:3103/crm/deals: Error: useCrmList must be used inside <CrmShell>
- **[high] console-error** — main http://127.0.0.1:3103/crm/deals: Error: useCrmList must be used inside <CrmShell>
- **[high] console-error** — main http://127.0.0.1:3103/crm/deals: Error: useCrmList must be used inside <CrmShell>
- **[high] console-error** — main http://127.0.0.1:3103/crm/leads: Failed to load resource: the server responded with a status of 403 (Forbidden)
