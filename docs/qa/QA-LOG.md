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
