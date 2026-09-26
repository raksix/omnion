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

## ISSUE-003 — Palette text/fill pairs fail WCAG AA (badge, buttons, chips) · medium · fixed

- **Screen:** every admin screen (sidebar badge, primary buttons, chips) and the public
  renderer's links
- **Found:** passes `20260926-101942` … `20260926-114713` — 5× `low-contrast`, e.g.
  `{"text":"O","ratio":3.9,"min":4.5,"fontSize":14}` (white on the accent fill of the sidebar
  badge). The walkthrough's own diagnostics name the rest of the set: `Selected` 4.18
  (`accent-strong` on `accent-soft`), `Active` 4.26 (`positive` on `positive-soft`), `Sign in`
  3.9 (white on the accent fill) and, on the site host, `Back to the home page` 3.7
  (`--mn-accent` on the cream canvas). One palette, five surfaces.
- **Root cause:** the terracotta the design language is built on (`#c96442`) is 3.9:1 against
  white — under AA for text — and both the panel and the theme reused it for text *and* for the
  fill behind white text.
- **Fix:** the palette's text carriers moved one step darker; nothing else changed.
  Admin (`apps/admin/app/globals.css`): `--color-accent #c96442 → #b0563a`,
  `--color-accent-strong #b0563a → #a34a2b`, `--color-positive #3f7d5c → #35704f`,
  `--color-caution #96661c → #8a5d16`. Theme (`themes/minimal/styles/minimal.css`):
  `--mn-accent #c96442 → #b0563a`, with the theme version bumped 0.1.0 → 0.1.1
  (`omnion.theme.json`, `package.json`). Ratios after the change: white on accent 4.95,
  accent-strong on accent-soft 4.96, positive on positive-soft 5.11, caution on caution-soft
  5.03, theme accent on cream 4.70.
- **Proof:** `qa-artifacts/20260926-115544` — all 8 walked screens report **0** contrast
  failures in `diagnostics.json` (5 before), the programmatic finding list dropped 10 → 5 (the
  remaining 5 are the web root's 404 console/request noise, the ISSUE-002 follow-up), and the
  vision review no longer sees the badge on any screenshot.

## ISSUE-004 — Next.js dev indicator covers the sidebar's “Sign out” row · medium · fixed

- **Screen:** every admin/web screen while the stack runs under `next dev`
- **Evidence:** `findings/vision.json` of `qa-artifacts/20260926-115544` — 7 of that pass's 8
  vision findings are this badge. It is rated **medium** on `page-overview`, `page-media` and
  `page-ai` (“covers the start of the text so it reads '…ign out'”) and low on `mobile-ai`,
  `mobile-overview`, `mobile-pages` and `web-first-link` (a stray “N” floating in the corner).
  It was hidden behind the contrast noise on earlier passes; with the palette clean it is the
  only thing the vision review still reports on the admin screens.
- **Root cause:** the floating `N` badge is Next.js' own development indicator. It is anchored to
  the bottom-left corner of the viewport, which is exactly where the panel's sidebar footer sits,
  so it lands on top of the “Sign out” row and travels into every screenshot.
- **Fix:** `devIndicators: false` in `apps/admin/next.config.ts` and `apps/web/next.config.ts`
  (development only — a production build renders no indicator, so nothing changes for a
  deployment).
- **Proof:** `qa-artifacts/20260926-122611` — the vision review reports **0 issues** across all 14
  screenshots (was 9: 5 medium + 4 low, every one of them this badge) and `summary.json` carries
  no mention of the badge; the walkthrough's own counters are unchanged (49 clicks · 8 fills ·
  49 shots · 5 programmatic findings, all of them the web root's 404 noise tracked below).

## ISSUE-005 — A disabled filled button keeps white text on a faded fill · low · fixed

- **Screen:** `/ai` — the “Try it” card's Send button; every `bg-accent … disabled:opacity-*`
  control followed the same rule (7 buttons across 5 files)
- **Evidence:** vision of `qa-artifacts/20260926-115544`, `page-ai`: “the Send button uses white
  text on a pale terracotta background”. The button is disabled in the QA fixture because no
  provider is connected, and `opacity-60` faded the whole control. The walkthrough's contrast
  pass cannot see it: its rule skips an element that has children, and this button carries an
  icon *and* a label.
- **Root cause:** the fade sat on the control as a whole, so the label was alpha-composited over
  the page — white text stayed white while the terracotta fill washed out underneath it. Measured
  on the live stack with `scripts/qa/probe-disabled-contrast.cjs`: fill `rgb(208,154,137)`, text
  `rgb(255,255,255)` → **2.43:1**.
- **Fix:** mute the disabled state instead of fading it —
  `disabled:bg-quiet-soft disabled:text-muted` replaces `disabled:opacity-60` (`disabled:opacity-50`
  in the media panel) on the filled accent buttons: `app/login/page.tsx`,
  `features/setup/setup-view.tsx`, `features/media/media-view.tsx`, `features/pages/pages-view.tsx`
  and `features/ai/ai-view.tsx` (×3). Nothing else about the controls changed.
- **Proof:** the probe reports fill `rgb(238,234,226)` with text `rgb(107,101,96)` → **4.79:1**
  (≥ 4.5), and with the pointer over the disabled control the fill stays `rgb(238,234,226)`: the
  `disabled:` variant wins over `hover:bg-accent-strong` in the compiled sheet, so hovering a
  disabled button no longer swaps in a dark fill. The pass after the change
  (`qa-artifacts/20260926-125712`) is unchanged — 49 clicks · 8 fills · 49 shots, vision 0 issues —
  and the only programmatic findings are the web root's 404 noise tracked above.

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
- The contrast pass skips a text element that has children (an icon next to a label is the common
  case), so it cannot see every label; vision covers those. Known blind spot that is *not*
  tracked as an issue yet: `placeholder:text-muted/70` (3.0:1) on the sign-in and wizard inputs —
  placeholders are not text nodes, so neither pass reports them. Candidate for a polish tick.
