/**
 * REQ-063 acceptance 7, the recovery half, in the order the full pass performs it.
 *
 * The isolated recovery probe proved the editor recovers from a heading-order warning
 * (warnings 1 -> 0, publish enabled throughout). The full pass records the same provoke
 * with `outlineWarningCleared`, `clearedAfterFix` and `publishEnabledAfterFix` all false. This
 * probe walks the pass's own sequence — heading, text, columns, image, reorder, duplicate,
 * delete, three columns, then the heading-order provoke — and reports the status bar at every
 * step, so the step that first turns `errors` from 0 to 1 is named rather than inferred.
 */
const { chromium } = require("playwright-core");

const ADMIN = process.argv[2] || "http://127.0.0.1:3101";
const OUT = { steps: [] };
const log = (...a) => console.log("[probe2]", ...a);

const record = async (page, name) => {
  const bar = await page
    .locator("[data-block-status]")
    .first()
    .evaluate((el) => ({
      errors: el.getAttribute("data-block-errors"),
      warnings: el.getAttribute("data-block-warnings"),
      count: el.getAttribute("data-block-count"),
    }))
    .catch(() => ({}));
  const issues = await page
    .locator("[data-block-issues] li")
    .allInnerTexts()
    .catch(() => []);
  const published = await page
    .locator("[data-block-publish]")
    .first()
    .isDisabled({ timeout: 5000 })
    .catch(() => null);
  const order = await page
    .locator("[data-block-canvas-block]")
    .evaluateAll((n) => n.map((x) => x.getAttribute("data-block-canvas-block")))
    .catch(() => []);
  const row = {
    step: name,
    ...bar,
    publishDisabled: published,
    order: order.join(","),
    issues: issues.map((t) => t.replace(/\s+/g, " ").trim().slice(0, 80)),
  };
  OUT.steps.push(row);
  log(
    `${name}: errors=${row.errors} warnings=${row.warnings} count=${row.count} ` +
      `publishDisabled=${row.publishDisabled} order=[${row.order}]`,
  );
  return row;
};

const insert = async (page, type) => {
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page.locator(`[data-block-insert-option=${type}]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(800);
};

(async () => {
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  const page = await browser.newPage({ viewport: { width: 1600, height: 1000 } });
  try {
    await page.goto(ADMIN, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(2500);
    if (/\/login|\/setup/.test(page.url())) {
      await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
      await page.waitForTimeout(700);
      await page.locator('input[type="email"]').first().fill("qa-owner@omnion.test");
      await page.locator('input[type="password"]').first().fill("OmnionQa-Passw0rd-2026!");
      await page.locator('form button[type="submit"]').first().click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(3500);
    }
    if (/\/login/.test(page.url())) throw new Error("could not sign in");

    // ---- create the page the way the pass does -------------------------------------------------
    await page.goto(`${ADMIN}/pages`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector("[data-page-new]", { timeout: 20000 });
    await page.locator("[data-page-new]").first().click({ timeout: 6000 });
    await page.waitForTimeout(800);
    await page.locator("#page-title").fill("QA recovery probe").catch(() => {});
    await page.locator("#page-slug").fill("qa-recovery-probe").catch(() => {});
    await page.locator("#page-body").fill("Pre-block text.").catch(() => {});
    await page.locator("[data-page-save]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(2400);

    const row = page.locator("tr", { hasText: "QA recovery probe" });
    const link = (await row.count()) > 0
      ? row.locator('a[href^="/pages/"][href$="/edit"]').first()
      : page.locator('a[href^="/pages/"][href$="/edit"]').first();
    if ((await link.count()) === 0) throw new Error("no editor link");
    await link.click({ timeout: 6000 });
    await page.waitForSelector("[data-block-editor]", { timeout: 20000 });
    await page.waitForTimeout(800);
    OUT.editorPath = page.url();

    // ---- 1. heading, text, columns ------------------------------------------------------------
    await insert(page, "heading");
    await page.locator("#block-prop-text").first().fill("QA heading from the walkthrough").catch(() => {});
    await page.waitForTimeout(800);
    await insert(page, "text");
    await page.locator("#block-prop-text").first().fill("A paragraph written by the QA walkthrough.").catch(() => {});
    await page.waitForTimeout(800);
    await insert(page, "columns");
    await page.waitForTimeout(800);
    await record(page, "1 three blocks inserted");

    // ---- 2. image block: the pass's known blocking source ---------------------------------------
    await insert(page, "image");
    await page.waitForTimeout(700);
    await record(page, "2 image inserted (empty)");

    const imgRow = page.locator("[data-block-canvas-block=image]").first();
    if ((await imgRow.count()) > 0) {
      await imgRow.click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(500);
    }
    // The pass fills the two fields the API requires. Which one is selected matters, so report it.
    OUT.srcFieldCount = await page.locator("#block-prop-src").count();
    OUT.altFieldCount = await page.locator("#block-prop-alt").count();
    await page
      .locator("#block-prop-src")
      .first()
      .fill("/api/v1/public/media/00000000-0000-0000-0000-000000000000")
      .catch(() => {});
    await page.waitForTimeout(600);
    await page.locator("#block-prop-alt").first().fill("A screenshot of the QA walkthrough").catch(() => {});
    await page.waitForTimeout(1200);
    await record(page, "3 image src+alt filled");
    OUT.selectedNow = (await page.locator("[data-block-inspector]").first().innerText().catch(() => ""))
      .replace(/\s+/g, " ")
      .trim()
      .slice(0, 100);

    // ---- 3. the columns flow, exactly as the pass drives it ------------------------------------
    const colsRow = page.locator("[data-block-canvas-block=columns]").first();
    if ((await colsRow.count()) > 0) {
      await colsRow.click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(600);
    }
    await record(page, "4 columns block selected");
    const add = page.locator("[data-block-add-column]").first();
    OUT.addColumnOffered = (await add.count()) > 0;
    OUT.addColumnDisabled = OUT.addColumnOffered ? await add.isDisabled().catch(() => null) : null;
    OUT.canAddTitle = OUT.addColumnOffered
      ? (await add.getAttribute("title").catch(() => null))
      : null;
    if (OUT.addColumnOffered && !OUT.addColumnDisabled) {
      await add.click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(1000);
    }
    await record(page, "5 after add-column attempt");
    OUT.columnCountAfter = await page
      .locator("[data-block-columns]")
      .first()
      .getAttribute("data-block-column-count")
      .catch(() => null);

    // ---- 4. heading-order provoke + fix --------------------------------------------------------
    await insert(page, "heading");
    await page.locator("#block-prop-text").first().fill("QA section heading").catch(() => {});
    await page.waitForTimeout(500);
    await page.locator("#block-prop-level").first().selectOption("h1").catch(() => {});
    await page.waitForTimeout(1200);
    const provoked = await record(page, "6 heading set to h1 (provoke)");
    OUT.outlineWarningShown = Number(provoked.warnings) > 0;
    OUT.outlineWarningIsNotBlocking = provoked.errors === "0";
    OUT.outlineWarningIsAdvisory = provoked.issues.some((t) => /h1 comes after|heading order/i.test(t));

    await page.locator("#block-prop-level").first().selectOption("h2").catch(() => {});
    await page.waitForTimeout(1200);
    const fixed = await record(page, "7 heading set back to h2 (the fix)");
    // The heading warning is gone; the page may still carry other advisories, and it must.
    OUT.outlineWarningCleared = !fixed.issues.some((t) => /h1 comes after|heading order|follows an h/i.test(t));
    OUT.remainingWarningsAfterFix = fixed.issues;
    OUT.publishEnabledAfterFix = fixed.publishDisabled === false;
    OUT.clearedAfterFix = String(fixed.errors) === "0";
    // A leftover warning must be a way INTO its block, not a dead end with a soft voice.
    const warnJump = page.locator("[data-block-first-warning]").first();
    OUT.warningJumpOffered = (await warnJump.count()) > 0;
    OUT.warningJumpText = (await warnJump.innerText().catch(() => "")).replace(/\s+/g, " ").trim();
    if (OUT.warningJumpOffered) {
      await warnJump.click({ timeout: 6000 }).catch(() => {});
      await page.waitForTimeout(700);
      OUT.warningReachable = (await page.locator("[data-block-issues] li").count()) > 0;
      OUT.warningIssuesAfterJump = (await page.locator("[data-block-issues] li").allInnerTexts().catch(() => []))
        .map((t) => t.replace(/\s+/g, " ").trim().slice(0, 70));
    }

    log("VERDICT", JSON.stringify({
      outlineWarningShown: OUT.outlineWarningShown,
      outlineWarningCleared: OUT.outlineWarningCleared,
      outlineWarningIsNotBlocking: OUT.outlineWarningIsNotBlocking,
      clearedAfterFix: OUT.clearedAfterFix,
      publishEnabledAfterFix: OUT.publishEnabledAfterFix,
      warningJumpOffered: OUT.warningJumpOffered,
      warningReachable: OUT.warningReachable,
    }));
  } catch (e) {
    OUT.fatal = String(e && e.message ? e.message : e);
    log("FATAL", OUT.fatal);
  } finally {
    await browser.close().catch(() => {});
  }
  console.log(JSON.stringify(OUT, null, 2));
})();
