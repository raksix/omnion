# Omnion QA — latest pass (w4)

- When: 2026-09-29T05:57:36.975Z · artifacts: `qa-artifacts/20260929-054500`
- Interactions: 689 clicks · 120 field fills · 35 form submissions · 842 screenshots
- Console errors: 368 · failed requests: 367 · dialogs: 1
- Programmatic findings: 869 (high 857 · medium 12 · low 0)
- Vision issues: 0

## Top findings

- **[medium] low-contrast** — sales-quotes: 1 text node(s) under WCAG AA, e.g. {"text":"0","ratio":1,"min":4.5,"fontSize":12}
- **[medium] low-contrast** — sales-orders: 1 text node(s) under WCAG AA, e.g. {"text":"0","ratio":1,"min":4.5,"fontSize":12}
- **[high] overflow-mobile** — mobile crm-contacts: horizontal overflow
- **[medium] offscreen-mobile** — mobile crm-contacts: 172 element(s) outside the viewport
- **[high] overflow-mobile** — mobile crm-companies: horizontal overflow
- **[medium] offscreen-mobile** — mobile crm-companies: 172 element(s) outside the viewport
- **[high] overflow-mobile** — mobile crm-deals-mobile: horizontal overflow
- **[medium] offscreen-mobile** — mobile crm-deals-mobile: 172 element(s) outside the viewport
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theSheetOpens
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theSheetNamesTheKeys
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theSheetAdvertisesDestinations
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theSheetReopensAfterNavigation
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: slashFocusesSearch
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theListHasRows
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: nCreates
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: gThenDGoesToDeals
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: thePhoneDefaultsToTheList
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theBoardIsStillOffered
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theBoardScrollsOnAPhone
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theStageHeaderSticks
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theStagesKeepTheirTotals
- **[high] crm-keyboard-mobile** — keyboard/mobile step failed: theFormIsOnAPhone
- **[high] crm-state** — state step failed: contacts_hasAState
- **[high] crm-state** — state step failed: contacts_readsAsASentence
- **[high] crm-state** — state step failed: contacts_showsTheRequestId
