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

## ISSUE-002 — The renderer showed a blank page for an unpublished address · high · fixed

- **Screen:** `http://qa.omnion.test:3200/` (web, via the host mapping in the walkthrough)
- **Found:** every pass from `20260926-101942` to `20260926-111110` — the renderer answered `404`
  with an **empty body**. The vision review of `web-home` saw a blank white page and the DOM read
  of the same URL had no heading, no text and no `.mn-shell`; only the RSC payload sat in a
  `<script>` tag.
- **Root cause:** the dev server blocks cross-origin dev resources for `qa.omnion.test`
  (“Blocked cross-origin request to Next.js dev resource /_next/hmr”), and the blocked HMR socket
  is also the transport for React's debug channel. Any route whose server render fails —
  `notFound()` on an unpublished address is the normal one — is then recovered in the browser
  (`__next_error__` document → `createRoot()`), and that recovery waits for the debug channel
  forever: nothing is ever painted. The not-found copy was never missing; it simply never reached
  the screen.
- **Fix:** `apps/web/next.config.ts` lists the QA host in `allowedDevOrigins` (development only).
  The socket connects, the fallback renders, and the site's not-found view (heading, explanation,
  link) is visible.
- **Proof:** `qa-artifacts/20260926-112300` — `summary.json → web.text` carries the not-found
  copy (“Nothing published here … Back to the home page”), the `web-home` screenshot shows the
  rendered view, and the vision review reports **0 issues** for `web-home` and `web-first-link`
  (a high-severity blank page on every earlier pass). Vision `high` count for the whole pass: 2 → 0.
- **Regression guard:** the walkthrough now records the root's visible text and raises a `high`
  `web-blank` finding when a `404` answer has none; the guard fires on the previous pass's
  summary and stays silent on the new one.
- **Follow-up (harness, open):** the root legitimately answers `404` because the fixture publishes
  no page with slug `home`. The walkthrough should publish that page too, so the site's front door
  is exercised like any other page and the root stops producing 404 noise.

## ISSUE-003 — Brand badge “O” fails contrast on every screen · medium · open

- **Screen:** all admin screens (sidebar + header)
- **Evidence:** `findings` of `qa-artifacts/20260926-112300` — 5× `low-contrast`, e.g.
  `{"text":"O","ratio":3.9,"min":4.5,"fontSize":14}` (white on the accent fill). Same set as the
  previous pass; the walkthrough's own diagnostics also see `Active`/`Selected`/`Sign in` labels
  at 4.18–4.26.
- **Fix direction:** darken the badge background (or enlarge/bolden the letter so it counts as
  large text) — the badge is the panel's only contrast failure.

## ISSUE-004 — Next.js dev indicator overlaps the sidebar footer · low · open

- **Screen:** every admin/web screen in development
- **Evidence:** `findings/vision.json` of `qa-artifacts/20260926-112300` — a dark “N” badge sits
  on top of the “Sign out” control on `page-sites`, `click-ai-18-dialog` and `click-ai-2-navigated`
  (vision rates it medium there, low on `mobile-ai`/`mobile-overview`). It is Next.js' own
  development indicator, not part of the UI, so it is a `low` defect for the product — but it does
  obscure a real control while the app runs under `next dev`.
- **Fix direction:** `devIndicators: false` in `apps/admin/next.config.ts` and
  `apps/web/next.config.ts` (development only; keeps QA screenshots clean).

## Harness notes (fixed, not app defects)

- The `/api/v1/ai/providers` 400 of the first passes is gone: the walkthrough's sample values
  now fill the form with a real base URL, so the newest passes show zero `high` findings and
  `Connect` → `ok`. Sample-value heuristics: `scripts/qa/walkthrough.cjs`.
- The public pass now checks the renderer's real job: it opens the page the panel published
  (`/qa-sample`) on the site's own host and requires a `200` with its heading, and it requires the
  not-found answer to show a page (see ISSUE-002). Both screenshots (`web-published`,
  `web-home`) are part of the vision review.
- `web-console`/`web-request` entries for `http://qa.omnion.test:3200/` are the renderer correctly
  answering “this address has no published page” — tracked by the ISSUE-002 follow-up, not app
  defects. The `_next/hmr` WebSocket failures are gone with the originating fix.
- Screenshots are captured from the QA stack (`omnion_qa` database, pm2 `omnion-qa-*`), so
  findings never depend on hand-made data.
