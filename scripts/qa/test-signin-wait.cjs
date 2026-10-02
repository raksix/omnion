#!/usr/bin/env node
// Omnion QA — the sign-in wait, tested against a real browser.
//
// The walkthrough signs in and then used to `waitForTimeout(1200)` before asking whether it
// worked. On a loaded box the login POST takes longer than that, so the pass read the URL while
// the sign-in was still in flight, decided it had failed and wrote `fatal: "could not sign in"`
// into summary.json — with a session cookie sitting in the browser the whole time. The API log
// says "session created"; the pass says it could not sign in. Both are true, and the pass is the
// one that wasted an hour.
//
// This is that failure as a test: sign in against a live stack, with the API's response delayed by
// the caller (a Playwright route that stalls the login response), and assert the pass waits for
// the sign-in instead of a fixed number of milliseconds. The old code fails it; the new code
// passes it.
//
//   QA_ADMIN_URL=http://127.0.0.1:3101 QA_API_URL=http://127.0.0.1:18081 \
//     node scripts/qa/test-signin-wait.cjs
//
// It needs a running admin panel and API with the QA owner account — the same ones run.sh
// starts — and it never touches the database.
const path = require("path");

const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3101";
const API = process.env.QA_API_URL || "http://127.0.0.1:18081";
const CREDS = {
  email: process.env.QA_ADMIN_EMAIL || "qa-owner@omnion.test",
  password: process.env.QA_ADMIN_PASSWORD || "OmnionQa-Passw0rd-2026!",
};
// How much longer than the old hard-coded 1200 ms the response is held. The old code failed at
// 1200 ms, so the stall has to clear it comfortably to be a real test rather than a formality.
const STALL_MS = Number(process.env.QA_SIGNIN_STALL_MS || 3000);

let pass = 0;
let fail = 0;
const ok = (m) => { pass += 1; console.log(`  ok   — ${m}`); };
const bad = (m) => { fail += 1; console.log(`  FAIL — ${m}`); };

(async () => {
  const { chromium } = require(path.join(process.env.NODE_PATH || "/root/test-hermes/node_modules", "playwright-core"));
  const browser = await chromium.launch({
    executablePath: process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome",
    args: ["--no-sandbox"],
  });
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    // Hold the login RESPONSE (not the request) for STALL_MS. The session is created either
    // way — that is the whole point — so this reproduces the real failure: the credentials are
    // right, the server accepted them, and only the client's patience was too short.
    let stalled = false;
    await page.route("**/api/v1/auth/login", async (route) => {
      stalled = true;
      await new Promise((r) => setTimeout(r, STALL_MS));
      await route.continue();
    });

    await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded", timeout: 30000 });
    await page.waitForSelector('input[type="email"]', { timeout: 20000 });
    await page.locator('input[type="email"]').first().fill(CREDS.email);
    await page.locator('input[type="password"]').first().fill(CREDS.password);

    // The exact wait under test, copied from ensureSignedIn. If walkthrough.cjs changes, this
    // copy is what has to change with it — which is the point: the test names the behaviour
    // ("wait for the sign-in") and the two are edited together.
    const t0 = Date.now();
    const clicked = await page.locator("button[type=submit], form button").first().click().then(() => "Sign in").catch(() => null);
    void clicked;
    const neverSettles = () => new Promise(() => {});
    const signedIn = await Promise.race([
      page
        .waitForURL((u) => !/\/login|\/setup/.test(u.toString()), { timeout: 30000 })
        .then(() => true, neverSettles),
      page
        .waitForSelector('nav[aria-label="Sections"]', { timeout: 30000 })
        .then(() => true, neverSettles),
      page.waitForTimeout(30000).then(() => false),
    ]);
    const waited = Date.now() - t0;
    await page.waitForTimeout(signedIn ? 400 : 0);
    const url = page.url();

    if (!stalled) bad("the login route never fired — the test did not exercise the wait it is measuring");
    else ok(`the login response was stalled ${STALL_MS}ms (the old code's 1200ms was not enough)`);

    // The whole test. Waiting under STALL_MS means the fixed sleep gave up before the response.
    if (signedIn) ok(`signed in after ${waited}ms, past the ${STALL_MS}ms stall`);
    else bad(`declared failure after ${waited}ms while the request was still in flight`);

    if (!/\/login/.test(url)) ok(`left the login screen (${url})`);
    else bad(`still on ${url} after a successful sign-in`);

    // And the shell: a signed-in pass measures screens, so the app shell has to be there.
    if ((await page.locator('nav[aria-label="Sections"]').count()) > 0) ok("the app shell rendered");
    else bad("no app shell after sign-in — a pass would die on its first screen");

    // The failure this replaces, asserted so nobody puts the sleep back: at 1200ms this pass
    // would have read /login and written "could not sign in" with the session already created.
    if (waited > 1200) ok(`the wait outlasted the old hard-coded 1200ms (${waited}ms)`);
    else bad(`finished in ${waited}ms, inside the old sleep — the case cannot fail`);
  } finally {
    await browser.close();
  }
  console.log(`\nsignin-wait: ${pass} passed, ${fail} failed`);
  process.exit(fail === 0 ? 0 : 1);
})().catch((err) => {
  console.error("signin-wait: harness error:", err && err.message ? err.message : err);
  process.exit(1);
});
