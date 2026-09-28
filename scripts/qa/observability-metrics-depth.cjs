#!/usr/bin/env node
/**
 * Omnion QA — the REQ-126 metric-catalogue depth pass, runnable on its own.
 *
 * The full `run.sh` walk visits every screen on the box, and on a machine where six other
 * writers are compiling and three are driving their own Chromium at the same time it dies
 * part-way with `Target page, context or browser has been closed` — long before this screen's
 * depth pass runs. A pass that aborts proves nothing, so this script drives exactly one depth
 * pass against one stack and prints the steps it recorded.
 *
 * The assertions are the walkthrough's own, not a re-implementation: `runObservabilityMetricsDepth`
 * is exported from `walkthrough.cjs`, so a fix to the screen's checks cannot drift away from what
 * the full pass would say. What lives here is only the harness — launch, sign in, capture.
 *
 *   QA_ADMIN_PORT=3105 node scripts/qa/observability-metrics-depth.cjs
 *
 * It reports what it found; it does not decide whether the screen passes. A run that dies
 * before the steps are written proves nothing, exactly like an aborted full pass.
 */
const path = require("path");
const fs = require("fs");

const NODE_PATH = process.env.NODE_PATH || "/root/test-hermes/node_modules";
const CHROME = process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const ADMIN = process.env.QA_ADMIN_URL || `http://127.0.0.1:${process.env.QA_ADMIN_PORT || 3105}`;
const OUT = path.resolve(
  process.env.QA_DEPTH_OUT ||
    path.join(process.cwd(), "qa-artifacts", `depth-${new Date().toISOString().replace(/[-:]/g, "").slice(0, 15)}`),
);

async function main() {
  fs.mkdirSync(path.join(OUT, "shots"), { recursive: true });

  // The walk writes its screenshots and click log into directories chosen by its own CLI flags, so
  // point those at this run's output before requiring it. `record` appends to `clicks.jsonl` in the
  // same place, which is how the steps survive a crash.
  process.env.QA_OUT = OUT;
  process.argv.push("--out", OUT);

  const { chromium } = require(path.join(NODE_PATH, "playwright-core"));
  const walk = require("./walkthrough.cjs");

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--js-flags=--max-old-space-size=512", "--disable-gpu"],
  });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();

  const consoleErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text().slice(0, 240));
  });

  try {
    // A freshly reset QA database has no installation, so the walkthrough's own wizard creates the
    // owner and then signs in. Reusing those two helpers is the point: a driver that signs in its
    // own way reaches the screen in a state the real pass never visits, and every assertion below
    // then measures the driver instead of the screen.
    const report = { steps: [], pages: [], mobile: [] };
    await walk.runWizard(page, report);
    const signedIn = await walk.ensureSignedIn(page, report);
    if (!signedIn) {
      throw new Error(`could not sign in to ${ADMIN} — the depth pass would measure the login page`);
    }

    await walk.runObservabilityMetricsDepth(page, report);

    const doc = {
      ranAt: new Date().toISOString(),
      admin: ADMIN,
      signedIn,
      steps: report.observabilityMetrics?.steps ?? [],
      mobile: report.observabilityMetrics?.mobile ?? [],
      consoleErrors: consoleErrors.length,
      consoleErrorSamples: consoleErrors.slice(0, 10),
      artifacts: path.relative(process.cwd(), OUT),
    };
    fs.writeFileSync(path.join(OUT, "depth.json"), JSON.stringify(doc, null, 2));
    console.log(JSON.stringify(doc, null, 2));
  } catch (err) {
    // A crash here is an infrastructure abort, not a verdict on the screen. Say so explicitly so
    // nobody reads the absence of findings as a pass.
    const doc = { fatal: String(err), ranAt: new Date().toISOString(), consoleErrors: consoleErrors.length };
    fs.writeFileSync(path.join(OUT, "depth.json"), JSON.stringify(doc, null, 2));
    console.error("depth pass aborted:", err);
    process.exitCode = 1;
  } finally {
    await browser.close();
  }
}

main();
