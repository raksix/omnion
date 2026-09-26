# QA issues

Open defects found by the walkthrough + vision review. One section per issue; `omnion-qa` fixes
at most one per tick and flips the status when a pass proves it gone. Severities: `high` = users
cannot do something / the screen is broken, `medium` = visible defect or accessibility failure,
`low` = polish / dev-only artifact.

---

## ISSUE-001 — Pages screen offers no way to create a page · high · open

- **Screen:** `/pages` (admin)
- **Evidence:** `qa-artifacts/20260926-101942/clicks.jsonl` — the whole screen exposes 10
  interactives: brand link, 5 nav links, site switcher, state filter, "Reload pages" and
  "Sign out". The empty state ("This site has no pages yet") carries **no call to action**, and
  `select count(*) from pages` is 0 after a full pass. Content is write-only through the API.
- **Impact:** the panel is read-only for content: pages/revisions (P05) cannot be created,
  edited, published or restored from the UI; the public renderer can therefore never serve
  anything (see ISSUE-002) and the publish path is untested end-to-end.
- **Fix direction:** add the create + edit + publish flow to `apps/admin/features/pages`
  (client of the existing `/api/v1/pages/*` routes): "New page" CTA in the empty state and the
  header, a form (title, slug, body), save draft, publish/unpublish, and a revision note.
  Keep it minimal but real — no dead buttons.

## ISSUE-002 — Public renderer serves nothing ("Nothing published here") · medium · open

- **Screen:** `http://qa.omnion.test:3200/` (web, via host mapping in the walkthrough)
- **Evidence:** `summary.json` → `web.status = 404`, title "Nothing published here"; vision
  review of `web-home` reports a blank page. Expected while no page exists — **blocked by
  ISSUE-001**.
- **Follow-up:** once a page can be published from the panel, the walkthrough must open it on
  the site host and confirm the theme renders title + body (that closes this issue).

## ISSUE-003 — Brand badge "O" fails contrast on every screen · medium · open

- **Screen:** all admin screens (sidebar + header)
- **Evidence:** `findings` in `summary.json` — 5× `low-contrast`, e.g. `{"text":"O","ratio":3.9,
  "min":4.5,"fontSize":14}` (white on the accent fill).
- **Fix direction:** darken the badge background (or enlarge/bolden the letter so it counts as
  large text) — the badge is the panel's only contrast failure.

## ISSUE-004 — Next.js dev indicator overlaps the sidebar footer · low · open

- **Screen:** every admin/web screen in development
- **Evidence:** vision review — `[medium] click-ai-1-navigated`: a dark "N" badge sits on top of
  the "Sign out" control; `[low] mobile-ai` shows the same artifact.
- **Fix direction:** `devIndicators: false` in `apps/admin/next.config.ts` and
  `apps/web/next.config.ts` (development only; keeps QA screenshots clean).

## Harness notes (fixed, not app defects)

- The `/api/v1/ai/providers` 400 in the first passes was the walkthrough filling the base-URL
  field with prose; a valid payload returns 201. Sample-value heuristics fixed in
  `scripts/qa/walkthrough.cjs` (URL-ish placeholders, model lists, API keys, selects,
  checkboxes).
- Screenshots are captured from the QA stack (`omnion_qa` database, pm2 `omnion-qa-*`), so
  findings never depend on hand-made data.
