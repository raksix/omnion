#!/usr/bin/env node
/**
 * Omnion QA — focused probe for the ⌘K palette (REQ-002, slice 2).
 *
 * The walkthrough proves the palette exists; this probe proves the parts a click-through cannot
 * see: that the input really takes focus when the palette opens, that the arrow keys move the
 * highlight (not only the mouse), that Enter opens the highlighted row, that a click on the
 * header box opens the palette, that the recent list remembers a query after a reload, and that
 * the phone layout is a full-screen sheet with 44px rows.
 *
 * Usage (the QA stack has to be up — `bash scripts/qa/run.sh` leaves it running):
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-palette.cjs
 *
 * Exit code 0 when every check passes, 1 otherwise; prints one line per check.
 */
"use strict";

const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3100";
const CHROME =
  process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const EMAIL = process.env.QA_OWNER_EMAIL || "qa-owner@omnion.test";
const PASSWORD = process.env.QA_OWNER_PASSWORD || "OmnionQa-Passw0rd-2026!";

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
}

async function signIn(page) {
  await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(500);
  if (page.url().includes("/login")) {
    await page.fill('input[name="email"]', EMAIL);
    await page.fill('input[name="password"]', PASSWORD);
    await page.click('button[type="submit"]');
    await page.waitForTimeout(1500);
  }
  return !page.url().includes("/login");
}

async function paletteState(page) {
  return page.evaluate(() => {
    const palette = document.querySelector("[data-search-palette]");
    if (!palette) {
      return { open: false };
    }
    const dialog = palette.querySelector("[role=dialog]");
    const rect = dialog ? dialog.getBoundingClientRect() : null;
    const rows = [...palette.querySelectorAll("[role=option]")];
    const input = palette.querySelector("[data-palette-input]");
    return {
      open: true,
      focusedInput: Boolean(input) && document.activeElement === input,
      activeDescendant: input ? input.getAttribute("aria-activedescendant") : null,
      optionIds: rows.map((row) => row.id),
      rowHeights: rows.map((row) => Math.round(row.getBoundingClientRect().height)),
      text: (palette.innerText || "").replace(/\s+/g, " ").slice(0, 400),
      sheet: rect
        ? { width: Math.round(rect.width), height: Math.round(rect.height), viewport: [innerWidth, innerHeight] }
        : null,
    };
  });
}

async function main() {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  // ---------------------------------------------------------------- desktop
  const desktop = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await desktop.newPage();
  const consoleErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text().slice(0, 160));
  });

  check("signed in", await signIn(page), page.url());
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);

  // ⌘K opens the palette and the input takes the focus.
  await page.keyboard.press("Control+K");
  await page.waitForTimeout(500);
  let state = await paletteState(page);
  check("Ctrl+K opens the palette", state.open);
  check("the input is focused on open", state.focusedInput);

  // A query answers with sections and rows.
  await page.fill("[data-palette-input]", "sample");
  await page.waitForTimeout(1200);
  state = await paletteState(page);
  const sections = await page.locator("[data-search-palette] [role=listbox] > div").count();
  check("results arrive", state.open && state.optionIds.length > 1, `${state.optionIds.length} options`);
  check("more than one section answers ‘sample’", sections > 1, `${sections} sections`);
  check("rows are at least 36px tall", state.rowHeights.every((height) => height >= 36), JSON.stringify(state.rowHeights.slice(0, 4)));
  const firstActive = state.activeDescendant;

  // The arrow keys move the highlight without the mouse.
  await page.locator("[data-palette-input]").press("ArrowDown");
  await page.waitForTimeout(200);
  const afterArrow = (await paletteState(page)).activeDescendant;
  check("ArrowDown moves the highlight", Boolean(afterArrow) && afterArrow !== firstActive, `${firstActive} → ${afterArrow}`);

  // Enter opens the highlighted row — a real screen, not a stub.
  await page.locator("[data-palette-input]").press("Enter");
  await page.waitForTimeout(1200);
  const openedUrl = page.url();
  check("Enter opens a screen", !/\/$/.test(new URL(openedUrl).pathname) && !(await paletteState(page)).open, openedUrl);

  // ⌘Enter opens the highlighted row in a new tab instead of this one.
  await page.keyboard.press("Control+K");
  await page.waitForTimeout(700);
  await page.fill("[data-palette-input]", "sample");
  await page.waitForTimeout(1100);
  const popupPromise = page.waitForEvent("popup", { timeout: 6000 }).catch(() => null);
  await page.locator("[data-palette-input]").press("Control+Enter");
  const popup = await popupPromise;
  check(
    "Ctrl+Enter opens the row in a new tab",
    Boolean(popup),
    popup ? popup.url() : "no popup opened",
  );
  if (popup) {
    await popup.close().catch(() => {});
  }
  await page.waitForTimeout(400);

  // A query nothing matches says so, and the box stays usable.
  await page.keyboard.press("Control+K");
  await page.waitForTimeout(700);
  await page.fill("[data-palette-input]", "zzzznothingmatches");
  await page.waitForTimeout(1200);
  const empty = await paletteState(page);
  check(
    "a query with no hits renders the no-results state",
    /Nothing matched/i.test(empty.text),
    empty.text.slice(0, 120),
  );
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);

  // Reopening shows the query under "Recent searches"; Escape closes from the input.
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(800);
  await page.keyboard.press("Control+K");
  await page.waitForTimeout(900);
  state = await paletteState(page);
  check("reopening shows the recent-search list", /Recent searches/i.test(state.text), state.text.slice(0, 120));
  check("the query is remembered", /\bsample\b/i.test(state.text));
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);
  check("Escape closes the palette", !(await paletteState(page)).open);

  // A click on the header box opens it too (the keyboard is not the only way in).
  await page.click("[data-search-box]");
  await page.waitForTimeout(600);
  check("a click on the box opens the palette", (await paletteState(page)).open);
  await page.keyboard.press("Escape");
  await page.waitForTimeout(300);

  // `/` focuses the box without opening anything.
  await page.locator("body").click({ position: { x: 5, y: 5 } }).catch(() => {});
  await page.keyboard.press("/");
  await page.waitForTimeout(300);
  const slash = await page.evaluate(() => ({
    focused: document.activeElement === document.querySelector("[data-search-box]"),
    open: Boolean(document.querySelector("[data-search-palette]")),
  }));
  check("/ focuses the box without opening the palette", slash.focused && !slash.open, JSON.stringify(slash));

  check("no console errors on the desktop pass", consoleErrors.length === 0, consoleErrors.slice(0, 2).join(" | "));

  // ---------------------------------------------------------------- mobile
  const mobile = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
    deviceScaleFactor: 2,
  });
  const mpage = await mobile.newPage();
  const mobileErrors = [];
  mpage.on("console", (msg) => {
    if (msg.type() === "error") mobileErrors.push(msg.text().slice(0, 160));
  });

  check("signed in on mobile", await signIn(mpage), mpage.url());
  await mpage.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await mpage.waitForTimeout(900);
  await mpage.click("[data-search-box]");
  await mpage.waitForTimeout(700);
  let mobileState = await paletteState(mpage);
  check("a tap on the box opens the palette on a phone", mobileState.open);
  await mpage.fill("[data-palette-input]", "sample").catch(() => {});
  await mpage.waitForTimeout(1100);
  mobileState = await paletteState(mpage);
  check(
    "the phone sheet fills the viewport",
    Boolean(mobileState.sheet) &&
      mobileState.sheet.width === mobileState.sheet.viewport[0] &&
      mobileState.sheet.height === mobileState.sheet.viewport[1],
    JSON.stringify(mobileState.sheet),
  );
  check(
    "phone rows are 44px",
    mobileState.rowHeights.length > 0 && mobileState.rowHeights.every((height) => height >= 44),
    JSON.stringify(mobileState.rowHeights.slice(0, 5)),
  );
  const closeButton = mpage.locator('[data-search-palette] button[aria-label="Close search"]').last();
  await closeButton.click({ force: true }).catch(() => {});
  await mpage.waitForTimeout(400);
  check("the visible close control closes the sheet", !(await paletteState(mpage)).open);
  check("no console errors on the mobile pass", mobileErrors.length === 0, mobileErrors.slice(0, 2).join(" | "));

  await browser.close();

  const failed = results.filter((row) => !row.ok);
  console.log(`\nPROBE ${failed.length === 0 ? "PASS" : "FAIL"}: ${results.length - failed.length}/${results.length} checks`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(`probe crashed: ${error && error.message}`);
  process.exit(1);
});
