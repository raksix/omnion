/**
 * A single-purpose probe for the menu editor: sign in, create a menu, open it, and report what
 * each screen actually rendered. It exists because the depth pass reports `editorReady: false`
 * with no reason, and "the editor did not load" has four different causes (a 404 on the route, a
 * crash in the tree, a refused permission, a client-side exception) that all look identical from
 * the outside.
 *
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-menu-editor.cjs
 */
const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3101";
const CREDS = {
  email: "qa-owner@omnion.test",
  password: "OmnionQa-Passw0rd-2026!",
};

async function main() {
  const browser = await chromium.launch({
    executablePath: process.env.QA_CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  const consoleErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text().slice(0, 300));
  });
  page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${String(error).slice(0, 300)}`));

  const report = { consoleErrors, steps: [] };
  const step = (name, value) => {
    report.steps.push({ name, ...value });
    console.log(`${name}: ${JSON.stringify(value)}`);
  };

  await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(1200);
  await page.locator('input[type="email"]').fill(CREDS.email).catch(() => {});
  await page.locator('input[type="password"]').fill(CREDS.password).catch(() => {});
  await page.locator('button:has-text("Sign in")').click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(2500);
  step("signed-in", { url: page.url() });

  await page.goto(`${ADMIN}/menus`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(2500);
  step("list", {
    ready: await page.locator("[data-menus-state=ready]").count(),
    error: await page.locator("[data-menus-state=error]").count(),
    empty: await page.locator("[data-menus-empty]").count(),
    rows: await page.locator("[data-menu-row]").count(),
    body: (await page.locator("body").innerText().catch(() => "")).slice(0, 400),
  });

  // Create one through the form, so the editor has a real id to open.
  await page.locator("[data-menus-create]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator("[data-menu-form-name]").fill("Probe menu").catch(() => {});
  await page.locator("[data-menu-form-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2500);
  const menuId = await page
    .locator("[data-menu-edit]")
    .first()
    .getAttribute("data-menu-edit")
    .catch(() => null);
  step("created", { menuId });

  if (menuId) {
    const response = await page.goto(`${ADMIN}/menus/${menuId}/edit`, {
      waitUntil: "domcontentloaded",
    });
    await page.waitForTimeout(3000);
    step("editor", {
      http: response?.status() ?? null,
      ready: await page.locator("[data-menu-editor-state=ready]").count(),
      error: await page.locator("[data-menu-editor-state=error]").count(),
      loading: await page.locator("[data-menu-editor-state=loading]").count(),
      treeEmpty: await page.locator("[data-menu-tree-empty]").count(),
      name: await page.locator("[data-menu-name]").inputValue().catch(() => null),
      body: (await page.locator("body").innerText().catch(() => "")).slice(0, 600),
    });

    // The three buttons that must work before anything else can be claimed.
    await page.locator("[data-menu-add-item]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(800);
    step("after-add", {
      treeRows: await page.locator("[data-menu-item-label]").count(),
      inspector: await page.locator("[data-menu-inspector]").count(),
      saveDisabled: await page.locator("[data-menu-save]").isDisabled().catch(() => null),
    });
  }

  await page.screenshot({ path: "/tmp/probe-menu-editor.png", fullPage: true });
  await browser.close();
  console.log("PROBE_JSON=" + JSON.stringify(report));
}

main().catch((error) => {
  console.error("probe failed:", error);
  process.exit(1);
});
