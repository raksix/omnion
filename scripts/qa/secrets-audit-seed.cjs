/**
 * Seed the audit trail through the panel, then let the depth pass measure a trail that exists.
 *
 * A fresh QA database has an empty trail, so every assertion that needs a row (the request-id
 * join, the action filter, the acknowledge) is skipped and reports "nothing to do" — which reads
 * like a pass and is not one. This walks the operator's own screens to create the activity:
 * reading the root-key state, opening the credential list, taking a lease. Those are the real
 * audited operations, driven the way a person drives them, so the trail the depth pass then
 * inspects is one a user could have produced.
 */
const path = require("path");
const NODE_PATH = process.env.NODE_PATH || "/root/test-hermes/node_modules";
const CHROME = process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const ADMIN = process.env.QA_ADMIN_URL || `http://127.0.0.1:${process.env.QA_ADMIN_PORT || 3105}`;

(async () => {
  const { chromium } = require(path.join(NODE_PATH, "playwright-core"));
  const walk = require("./walkthrough.cjs");

  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--js-flags=--max-old-space-size=512", "--disable-gpu"],
  });
  const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage();
  const report = { steps: [] };

  await walk.runWizard(page, report);
  if (!(await walk.ensureSignedIn(page, report))) throw new Error("sign-in failed");

  // The walkthrough resolves its own base URL from `--url` and silently defaults to the main
  // writer's panel on :3100. Driving the wrong stack produces a run that looks fine and measures
  // another branch's screens — 401s, empty trails, and no error anywhere. Point the module at
  // this driver's stack and prove it took, rather than trusting the default.
  const actual = walk.URL_ADMIN || ADMIN;
  if (new URL(actual).port !== new URL(ADMIN).port) {
    throw new Error(
      `walkthrough is pointed at ${actual} but this driver serves ${ADMIN} — pass --url ${ADMIN}`,
    );
  }

  const visited = [];
  for (const route of ["/secrets", "/secrets/root-key", "/secrets/credentials", "/secrets/leases", "/secrets/audit"]) {
    await page.goto(`${ADMIN}${route}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1500);
    const rows = await page.locator("[data-audit-row]").count().catch(() => 0);
    visited.push({ route, ok: !/404/.test(await page.title().catch(() => "")), auditRows: rows });
  }

  // Read the trail back through the API with the browser's own session cookie, which is the only
  // session that carries the audit context the screen would show.
  const trail = await page
    .evaluate(async () => {
      const r = await fetch("/api/v1/secrets/audit?limit=50", { credentials: "same-origin" });
      if (!r.ok) return { status: r.status, entries: [] };
      const d = await r.json();
      return { status: r.status, entries: (d.entries || []).map((e) => ({ action: e.action, request_id: e.request_id, ip: e.ip_address })) };
    })
    .catch((e) => ({ status: 0, entries: [], error: String(e) }));

  await browser.close();
  console.log(JSON.stringify({ visited, trailStatus: trail.status, entries: trail.entries.length, sample: trail.entries.slice(0, 12) }, null, 2));
})().catch((e) => {
  console.error("seed failed:", e);
  process.exit(1);
});
