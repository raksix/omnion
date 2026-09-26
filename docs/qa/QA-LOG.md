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
