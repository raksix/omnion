# QA log

## 2026-09-26 — harness bootstrap (chat session)

- Built the QA system: `scripts/qa/{run.sh, reset-db.sh, walkthrough.cjs, vision-review.cjs,
  summarize.cjs}`; run it with `bash scripts/qa/run.sh`.
- Walkthrough coverage: first-run wizard → sign-in → every admin screen (`/`, `/pages`,
  `/media`, `/sites`, `/ai`) → every visible control clicked (buttons, links, inputs, selects,
  checkboxes, file inputs) → sign-out/sign-in → mobile viewport pass → public renderer on the
  site host. Screenshots at every step; console errors, failed requests and dialogs recorded.
- Vision review: `deepseek/deepseek-v4.1-flash` reads each screenshot and reports real visual
  defects only (clipped text, overlaps, broken layout, invisible text, i18n keys).
- First full pass `20260926-101942`: 5 screens, 40 interactions (14 fills, 9 selects, 1 check),
  47 screenshots, 13 programmatic findings (3 high) + 3 vision issues (1 high).
- Seeded [`ISSUES.md`](ISSUES.md) with 4 defects (ISSUE-001…ISSUE-004); the `omnion-qa` cron
  (id `7eeb8f90c9e1`, every 15m) takes over from here — one fix per tick, re-verified by the next
  pass.
- Harness lessons from the bootstrap: a navigation kills element handles (re-inventory every
  round instead of indexing once); selects need `selectOption`, not `fill`; the public renderer
  needs the site host (`--host-resolver-rules=MAP qa.omnion.test 127.0.0.1`) or it correctly
  404s every request.

## 2026-09-26 · pass `20260926-104535` — ISSUE-001 fixed (pages write flow)

- Counters: 49 clicks · 8 field fills · 47 screenshots · 10 programmatic findings
  (high 0 · medium 10 · low 0) · vision review 14 shots → 7 issues (high 1 · medium 4 · low 2).
  Every `high` finding of the first passes is gone (the AI `Connect` 400 was a walkthrough
  sample-value bug, fixed in the harness).
- Fixed **ISSUE-001** (high): the `/pages` screen can now create, edit and publish pages —
  “New page” in the header and the empty state, a title/slug/body editor, “Edit” (prefilled,
  appends a draft revision) and “Publish” on every row, plus the slug fallback from the title.
  Files: `apps/admin/features/pages/pages-view.tsx`, `apps/admin/lib/api.ts`.
- Proof in the same pass: the walkthrough fills the editor and runs `Create page` → `Edit QA
  Sample Page` → `Save draft` → `Publish QA Sample Page`, all `ok`, no console error, no failed
  request; the QA database holds `qa-sample | published` with revision 1 `archived` and revision
  2 `published`. A focused probe of the flow passes 10/10.
- Still open: ISSUE-002 (the renderer's root is empty until a page with slug `home` is
  published — next tick extends the walkthrough to open the published page), ISSUE-003 (badge
  contrast), ISSUE-004 (Next.js dev indicator over the sidebar footer).
- Probe lesson: the walkthrough treats a repeated descriptor as already-clicked, so a form must
  never put a close/icon-only control *before* its fields — the first unseen key would close the
  form before it is filled. The editor keeps its actions after the fields for that reason.

## 2026-09-26 · pass `20260926-112300` — ISSUE-002 fixed (blank page at an unpublished address)

- Counters: 49 clicks · 8 field fills · 49 screenshots · 10 programmatic findings
  (high 0 · medium 10 · low 0) · vision review 14 shots → 5 issues (high 0 · medium 3 · low 2).
  Vision `high` went 2 → 0 and the blank-page report disappeared.
- Fixed **ISSUE-002** (high): the renderer painted nothing at an unpublished address. The dev
  server blocks cross-origin dev resources for `qa.omnion.test`, and the blocked HMR socket is
  also React's debug-channel transport — so the client-side recovery of a failed server render
  (`__next_error__` → `createRoot()`) waited for that channel forever. `apps/web/next.config.ts`
  now declares the QA host in `allowedDevOrigins` (development only); the not-found view renders
  and the socket connects.
- Harness: the public pass now verifies the renderer's real job — it opens the published page
  (`/qa-sample`, created by the panel pass in the same tick) on the site's own host and requires
  `200` + heading, and it records the root's visible text with a new `high` `web-blank` finding
  when a `404` answer shows no page. Negative control: the guard fires on the previous pass's
  summary (`web.text` empty) and stays silent on this one.
- Proof: `summary.json → web` — root text “Nothing published here … Back to the home page”;
  `published` = `200`, title “QA Sample Page · QA Site”, heading “QA Sample Page”, body text
  present, no overflow/broken images; vision review 0 issues for `web-home`, `web-first-link`
  and `web-published`.
- Still open: ISSUE-003 (badge contrast, 3.9), ISSUE-004 (dev indicator over “Sign out” — vision
  still sees it as medium on `page-sites`/`click-ai-*`), and the ISSUE-002 follow-up: publish a
  `home` page in the fixture so the root is real content instead of the expected `404`.
- Lesson: a Next.js dev-only failure mode can look exactly like an app defect — the giveaway is
  the `__next_error__` document plus a `<script>` payload holding the very content that never
  reached the screen. Measure `document.body.innerText`, not the response body.

## 2026-09-26 · pass `20260926-115544` — ISSUE-003 fixed (palette contrast)

- Counters: 49 clicks · 8 field fills · 49 screenshots · 5 programmatic findings
  (high 0 · medium 5 · low 0) · vision review 14 shots → 8 issues (high 0 · medium 3 · low 5).
  The contrast findings went 5 → 0; the five that remain are the web root's 404
  console/request noise (the ISSUE-002 follow-up).
- Fixed **ISSUE-003** (medium): the palette's text carriers sat one step too light for AA. The
  terracotta the design language is built on (`#c96442`) is 3.9:1 against white, and both the
  panel and the theme used it for white-on-fill controls (sidebar badge, every primary button)
  *and* as link text on the cream canvas (3.7:1); the `accent-strong` / `positive` / `caution`
  chips measured 4.18–4.36. The admin palette now uses `#b0563a` / `#a34a2b` / `#35704f` /
  `#8a5d16` and the Minimal theme's accent moved to `#b0563a` (theme patch 0.1.1); every
  text/fill pair in use is now ≥ 4.7:1.
- Proof: `20260926-115544` — all 8 walked screens report 0 contrast failures in
  `diagnostics.json`, the badge is gone from the vision review, and the public renderer still
  renders (`web-home` / `web-published`: 0 vision issues, published page 200 with its heading).
- Still open: ISSUE-004 (the Next.js dev badge now dominates the vision list — 7 of 8 findings,
  medium on three admin screens; `devIndicators: false` is next tick's fix), ISSUE-005 (new,
  low: a disabled filled button fades to white-on-pale-terracotta and its label is unreadable),
  and the ISSUE-002 follow-up (publish a `home` page so the root stops answering 404).
- Lesson: the walkthrough's contrast pass skips an element that has children, so a button whose
  label sits next to an icon is invisible to it — vision saw what the programmatic pass could
  not. Compare the two lists before calling a finding unreproducible.

## 2026-09-26 · pass `20260926-122611` — ISSUE-004 fixed (framework dev indicator)

- Counters: 49 clicks · 8 field fills · 49 screenshots · 5 programmatic findings
  (high 0 · medium 5 · low 0) · vision review 14 shots → **0 issues** (was 9: 5 medium + 4 low).
  The five programmatic findings are unchanged and are the web root's 404 console/request noise
  (the ISSUE-002 follow-up), not app defects.
- Fixed **ISSUE-004** (medium): the floating `N` badge Next.js draws in development is anchored to
  the bottom-left of the viewport — exactly where the panel's sidebar footer sits — so it covered
  the start of the “Sign out” label (“…ign out”) on every admin screenshot and floated in the
  corner of the mobile and public captures. `devIndicators: false` in `apps/admin/next.config.ts`
  and `apps/web/next.config.ts` (development only; a production build renders no indicator).
- Proof: `20260926-122611` — every one of the 14 reviewed screenshots reports 0 issues and the
  walkthrough counters are identical to the previous pass (49/8/49), so nothing else moved with
  the change.
- Still open: ISSUE-005 (low — a disabled `bg-accent` button fades to white-on-pale-terracotta),
  and the ISSUE-002 follow-up (the fixture should publish a `home` page so the root stops
  answering 404 and its five noise findings disappear).
- Lesson: a framework's own dev overlay can hide a real control, and it is invisible to the
  programmatic pass (it is not part of the DOM the diagnostics walk) while the vision review
  reports it on every shot — a finding repeated across unrelated screens is usually an overlay,
  not five layout bugs.

## 2026-09-26 · pass `20260926-125712` — ISSUE-005 fixed (disabled filled controls)

- Counters: 49 clicks · 8 field fills · 49 screenshots · 5 programmatic findings
  (high 0 · medium 5 · low 0 — all of them the web root's 404 noise tracked by the ISSUE-002
  follow-up) · vision review 14 shots → **0 issues**.
- Fixed **ISSUE-005** (low): a filled accent button in its disabled state faded the *whole*
  control (`disabled:opacity-60`; `disabled:opacity-50` in the media panel), so the label was
  alpha-composited over the page as white-on-pale-terracotta. Seven buttons across five files now
  mute instead of fade (`disabled:bg-quiet-soft disabled:text-muted`): login, the wizard, media,
  pages and the three AI-hub buttons.
- Proof: the vision review is nondeterministic about this one (it reported the button once and
  then went quiet), so the fix is proven with a focused probe instead —
  `scripts/qa/probe-disabled-contrast.cjs` measures the rendered pair on the live stack: fill
  `rgb(208,154,137)` with white text (2.43:1) before → fill `rgb(238,234,226)` with
  `rgb(107,101,96)` text (4.79:1) after, and the fill survives a hover (the `disabled:` variant
  outranks `hover:bg-accent-strong` in the compiled sheet).
- Still open: the ISSUE-002 follow-up (the fixture should publish a `home` page so the root stops
  answering 404 and its five noise findings disappear). ISSUE-001 … ISSUE-005 are all fixed.
- Lesson: when the vision model reports something once and then goes quiet, the silence is not a
  fix — measure the rendered result instead. A small probe that composites an element's opacity
  over its ancestor background turns a flaky visual hunch into a number, and its exit code becomes
  a regression guard the next pass can lean on.
