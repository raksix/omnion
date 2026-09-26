# QA — visual + interaction loop

The QA system boots a disposable Omnion stack, drives it with a real browser (Chromium via
Playwright), screenshots every screen, presses **every** visible button / link / input, and then
sends the screenshots to a vision model for a visual-defect review. Findings are written to
`docs/qa/` and consumed by the `omnion-qa` loop, which fixes one issue per tick.

## Run a pass

```bash
bash scripts/qa/run.sh
```

One pass does:

1. `scripts/qa/reset-db.sh` — drops and recreates the QA database `omnion_qa` (the walkthrough
   always starts from a fresh installation, so the first-run wizard and every empty state are
   exercised on every pass). Never point this at the development database.
2. Ensures API (`:18080`), admin panel (`:3100`) and public renderer (`:3200`) are up under pm2
   (`omnion-qa-api` / `omnion-qa-admin` / `omnion-qa-web`). All three are pinned to the QA
   database; the dev stack on `~/.env` is untouched.
3. `scripts/qa/walkthrough.cjs` — first-run wizard → sign-in → every admin screen → all controls
   → sign-out/sign-in → mobile viewport pass → public renderer.
4. `scripts/qa/vision-review.cjs` — a vision model reviews every screenshot for real visual
   defects (clipped text, overlaps, broken layout, invisible text, raw i18n keys, …).
5. Writes `qa-artifacts/<timestamp>/report.md` and refreshes [`docs/qa/QA-LATEST.md`](QA-LATEST.md).

## Artifacts (`qa-artifacts/<ts>/`, gitignored)

| File | Contents |
|---|---|
| `shots/*.png` | page screenshots, interesting interactions, mobile |
| `clicks.jsonl` | one line per interaction: label, outcome, console/network deltas |
| `diagnostics.json` | per-page DOM health: overflow, contrast, broken images, labels, ids |
| `findings/vision.json` | visual-review verdicts per screenshot |
| `report.md` / `summary.json` | human + machine roll-up |

## Conventions

- `scripts/qa/*` is the only place walkthrough knowledge lives — never teach it to skip work.
- Screenshots never enter git (see `.gitignore`); `docs/qa/*.md` do.
- The environment knobs: `QA_API_PORT`, `QA_ADMIN_PORT`, `QA_WEB_PORT`, `QA_CHROME`,
  `QA_NODE_PATH`, `VISION_MCP_API_KEY` / `VISION_MCP_BASE_URL` / `VISION_MCP_MODEL`.
- Chromium comes from `/root/.cache/ms-playwright/chromium-1234`; `playwright-core` is resolved
  through `NODE_PATH` (the repo does not depend on it).

## The `omnion-qa` loop

The cron job `omnion-qa` runs a pass every tick, books findings into [`ISSUES.md`](ISSUES.md),
fixes **at most one** issue per tick, re-runs the pass to prove it, and commits + pushes. It is
the only code writer while `omnion-build` is paused. See [`QA-LOG.md`](QA-LOG.md) for history.
