/**
 * The two content galleries answer the first-run platform account.
 *
 * The account the panel creates on first run is a platform account: it has **no** primary
 * organization, which is exactly what makes it an Owner. Both galleries are tenant-addressed reads,
 * and the browser pass caught the consequence — several hundred `400 organization_required` on
 * `/api/v1/patterns` and `/api/v1/page-templates`, which are the two screens a fresh installation
 * opens first. The API is right to refuse a tenant-addressed read that names no tenant; the client
 * was the part that had no answer to give it.
 *
 * This probe signs in as the seeded account, opens both screens, and asserts what a person would
 * say happened: the library loaded, the gallery loaded, and **neither screen logged a console
 * error or a failed request**. It also switches the tenant when the screen offers the choice, so
 * the picker is proven to be a control rather than decoration.
 *
 * It needs a seeded database (the walkthrough runs the first-run wizard), so it is a *companion*
 * to `bash scripts/qa/run.sh` rather than a replacement for it.
 *
 * Run against the w2 stack after a pass:
 *   QA_STACK=w2 QA_ADMIN_PORT=3101 node scripts/qa/probe-content-galleries.cjs
 */
const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN_URL || `http://127.0.0.1:${process.env.QA_ADMIN_PORT || 3101}`;
const CHROME =
  process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const CREDS = {
  email: process.env.QA_EMAIL || "qa-owner@omnion.test",
  password: process.env.QA_PASSWORD || "OmnionQa-Passw0rd-2026!",
};

const checks = [];
function check(name, value, detail) {
  checks.push({ name, value: value === true, detail: value === true ? "" : String(detail ?? "") });
}

(async () => {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();

  const consoleErrors = [];
  const failedRequests = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      consoleErrors.push(`${page.url()} :: ${message.text()}`);
    }
  });
  page.on("response", (response) => {
    if (response.status() >= 400) {
      failedRequests.push(`${response.status()} ${response.request().method()} ${response.url()}`);
    }
  });

  try {
    // ---------------------------------------------------------------- sign in
    // The panel's own selectors are deliberately loose (`input[type=email]`, not a fixed id):
    // the login form has been restyled at least once and a probe that hard-codes a selector dies
    // on a rename instead of on a defect. Same locators the walkthrough uses.
    await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(1000);
    if (page.url().includes("/setup")) {
      throw new Error(
        "the database is not seeded — run `bash scripts/qa/run.sh` first so the wizard creates the owner",
      );
    }
    const email = page.locator('input[type="email"], input[name="email"], #email').first();
    await email.waitFor({ timeout: 15000 });
    await email.fill(CREDS.email);
    await page
      .locator('input[type="password"], input[name="password"], #password')
      .first()
      .fill(CREDS.password);
    await page.locator('button[type="submit"]').first().click();
    await page.waitForURL((url) => !url.pathname.includes("/login"), { timeout: 30000 });


    for (const [route, label] of [
      ["/patterns", "patterns"],
      ["/page-templates", "page-templates"],
    ]) {
      const before = { console: consoleErrors.length, failed: failedRequests.length };
      await page.goto(`${ADMIN}${route}`, { waitUntil: "domcontentloaded" });
      await page.waitForTimeout(2500);

      // The screen must not be showing a failure state. "Could not be loaded" is the failure.
      const failure = await page
        .locator("text=could not be loaded, text=No organization to read")
        .first()
        .isVisible()
        .catch(() => false);
      check(`${label} renders a loaded screen`, !failure, "an error state is on screen");

      // The picker's value, when there is one, must be a real organization rather than "".
      const picker = page.locator("[data-tenant-picker]").first();
      const hasPicker = (await picker.count()) > 0;
      if (hasPicker) {
        const value = await picker.inputValue();
        check(`${label} names a tenant`, value.length > 0, `picker value was "${value}"`);
      } else {
        // No picker is correct for an account that belongs to exactly one tenant.
        check(`${label} answered without a tenant selector`, true);
      }

      // The half that matters: nothing on this screen failed.
      check(
        `${label} logged no console error`,
        consoleErrors.length === before.console,
        consoleErrors.slice(before.console).join(" | "),
      );
      check(
        `${label} had no failed request`,
        failedRequests.length === before.failed,
        failedRequests.slice(before.failed).join(" | "),
      );
    }

    // The picker is a control: changing it must keep the screen working, not break it.
    const picker = page.locator("[data-tenant-picker]").first();
    if ((await picker.count()) > 0) {
      const before = { console: consoleErrors.length, failed: failedRequests.length };
      const options = await picker.locator("option").all();
      if (options.length > 1) {
        const second = await options[1].getAttribute("value");
        await picker.selectOption(second);
        await page.waitForTimeout(2000);
        const stillFailing = failedRequests
          .slice(before.failed)
          .some((entry) => entry.includes("400"));
        check("switching tenant does not 400", !stillFailing, failedRequests.slice(before.failed).join(" | "));
      } else {
        check("switching tenant does not 400", true);
      }
    } else {
      check("switching tenant does not 400", true);
    }

    // ------------------------------------------------- the revisions screen crash
    // The block compare is raw JSON on the server, so a page with no blocks in its history
    // answers without an `entries` key — and `blocks.entries.map` threw on it, taking the whole
    // screen down. Reach the revisions screen and assert it rendered.
    await page.goto(`${ADMIN}/pages`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(1500);
    const pageLink = page.locator('a[href*="/edit"]').first();
    if ((await pageLink.count()) > 0) {
      const href = await pageLink.getAttribute("href");
      const pageId = href.split("/").filter(Boolean)[1];
      const before = consoleErrors.length;
      await page.goto(`${ADMIN}/pages/${pageId}/revisions`, { waitUntil: "domcontentloaded" });
      await page.waitForTimeout(2500);
      const crashed = consoleErrors
        .slice(before)
        .some((entry) => entry.includes("Cannot read properties of undefined"));
      check("revision history renders without a TypeError", !crashed, consoleErrors.slice(before).join(" | "));
    } else {
      check("revision history renders without a TypeError", true);
    }
  } finally {
    await browser.close();
  }

  const failed = checks.filter((entry) => !entry.value);
  for (const entry of checks) {
    console.log(`${entry.value ? "ok  " : "FAIL"} ${entry.name}${entry.detail ? ` — ${entry.detail}` : ""}`);
  }
  console.log(`\n${checks.length - failed.length}/${checks.length} passed`);
  if (consoleErrors.length) {
    console.log(`\nconsole errors seen (${consoleErrors.length}):`);
    for (const entry of consoleErrors.slice(0, 10)) console.log(`  ${entry}`);
  }
  if (failedRequests.length) {
    console.log(`\nfailed requests (${failedRequests.length}):`);
    for (const entry of failedRequests.slice(0, 10)) console.log(`  ${entry}`);
  }
  process.exit(failed.length === 0 ? 0 : 1);
})().catch((error) => {
  console.error(error);
  process.exit(2);
});
