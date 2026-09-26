# QA issues

Open defects found by the walkthrough + vision review. One section per issue; `omnion-qa` fixes
at most one per tick and flips the status when a pass proves it gone. Severities: `high` = users
cannot do something / the screen is broken, `medium` = visible defect or accessibility failure,
`low` = polish / dev-only artifact.

---

## ISSUE-001 — Pages screen offers no way to create a page · high · fixed

- **Screen:** `/pages` (admin)
- **Found:** `qa-artifacts/20260926-101942` — the screen exposed 10 interactives and no call to
  action; `select count(*) from pages` was 0 after a full pass.
- **Fix:** the panel now carries the write side of the content model:
  `apps/admin/features/pages/pages-view.tsx` + `createPage`/`updatePage`/`publishPage` in
  `apps/admin/lib/api.ts`. A “New page” control sits in the header and in the empty state, the
  editor (title, slug, body) creates a page as its first draft revision, “Edit” reopens it
  prefilled for a new draft revision, and “Publish” puts the draft live. The slug is derived
  from the title while the field is empty; the list keeps saying which revision is live and
  which is waiting.
- **Proof:** `qa-artifacts/20260926-104535` — the walkthrough clicks `New page` → fills title,
  slug and body → `Create page` → `Edit QA Sample Page` → `Save draft` → `Publish QA Sample
  Page`, every step `ok` with no console error and no failed request. The QA database then
  carries the page: `qa-sample | published` with revision 1 `archived` and revision 2
  `published`. A focused probe of the same flow passes 10/10.

## ISSUE-002 — Public renderer serves nothing on its root (“Nothing published here”) · medium · open

- **Screen:** `http://qa.omnion.test:3200/` (web, via host mapping in the walkthrough)
- **Evidence:** `qa-artifacts/20260926-104535/summary.json` → `web.status = 404`, title
  “Nothing published here”; the vision review of `web-home` sees a blank page. The renderer
  serves the site's `home` page at `/`; the walkthrough publishes a page under `qa-sample`, so
  the root legitimately has nothing yet — the blank root is expected until the walkthrough (or a
  user) publishes a page whose slug is `home`.
- **Next step (harness follow-up, unblocked now that pages can be published):** the walkthrough
  should open the published page on its own host (`/qa-sample` after the panel pass) and confirm
  the theme renders the title + body; that closes this issue. No app defect is known behind it.

## ISSUE-003 — Brand badge “O” fails contrast on every screen · medium · open

- **Screen:** all admin screens (sidebar + header)
- **Evidence:** `findings` of `qa-artifacts/20260926-104535` — 5× `low-contrast`, e.g.
  `{"text":"O","ratio":3.9,"min":4.5,"fontSize":14}` (white on the accent fill).
- **Fix direction:** darken the badge background (or enlarge/bolden the letter so it counts as
  large text) — the badge is the panel's only contrast failure.

## ISSUE-004 — Next.js dev indicator overlaps the sidebar footer · low · open

- **Screen:** every admin/web screen in development
- **Evidence:** vision review of `qa-artifacts/20260926-104535` — a dark “N” badge sits on top of
  the “Sign out” control on `page-overview`, `page-sites` and both `click-ai-*` shots (medium);
  `page-media` and `mobile-overview` show the same artifact (low).
- **Fix direction:** `devIndicators: false` in `apps/admin/next.config.ts` and
  `apps/web/next.config.ts` (development only; keeps QA screenshots clean).

## Harness notes (fixed, not app defects)

- The `/api/v1/ai/providers` 400 of the first passes is gone: the walkthrough's sample values
  now fill the form with a real base URL, so the two newest passes (`20260926-103507`,
  `20260926-104535`) show zero `high` findings and `Connect` → `ok`. Sample-value heuristics:
  `scripts/qa/walkthrough.cjs`.
- The `web-console` 404s and the `_next/hmr` WebSocket failure on `qa.omnion.test:3200` are dev
  server artifacts of the renderer pass (the 404 is the renderer correctly answering “no home
  page”, tracked as ISSUE-002); they are recorded, not treated as app defects.
- Screenshots are captured from the QA stack (`omnion_qa` database, pm2 `omnion-qa-*`), so
  findings never depend on hand-made data.
