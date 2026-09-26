#!/usr/bin/env node
/**
 * Omnion QA — focused probe for the command centre (REQ-032, slice 1).
 *
 * The walkthrough proves the palette's command group, the prefixes and a command that really
 * runs; this probe proves the parts a click-through cannot see on its own — that "Create a page"
 * lands on the form with the field focused, that running a command is written to the account's
 * own history (read back through the API, not the DOM), that clearing the history empties the
 * group immediately and after a reload, that a prefix narrows the answer to its own kind, and
 * that `Tab` walks the sections without the mouse.
 *
 * Usage (the QA stack has to be up — `bash scripts/qa/run.sh` leaves it running):
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-command-center.cjs
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
    await page.waitForTimeout(1600);
  }
  return !page.url().includes("/login");
}

/** Open the palette from the keyboard and return whether it is there. */
async function openPalette(page) {
  await page.keyboard.press("Control+K");
  await page.waitForSelector("[data-search-palette]", { timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  return (await page.locator("[data-search-palette]").count()) > 0;
}

const paletteText = (page) =>
  page
    .locator("[data-search-palette]")
    .first()
    .innerText()
    .then((text) => text.replace(/\s+/g, " ").trim())
    .catch(() => "");

const activeRow = (page) =>
  page
    .evaluate(
      () =>
        document.querySelector("[data-palette-input]")?.getAttribute("aria-activedescendant") ??
        null,
    )
    .catch(() => null);

async function main() {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  const consoleErrors = [];
  const serverErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") {
      const where = msg.location?.().url ?? "";
      consoleErrors.push(`${msg.text().slice(0, 120)}${where ? ` @ ${where}` : ""}`.slice(0, 200));
    }
  });
  page.on("response", (response) => {
    if (response.status() >= 500) {
      serverErrors.push(`${response.status()} ${response.url()}`);
    }
  });

  check("signed in", await signIn(page), page.url());

  // ---------------------------------------------------------------- the create command's landing
  // `/pages?new=1` is where "Create a page" lands: the form is open and its first field already
  // has the focus, with no palette left over the screen.
  await page.goto(`${ADMIN}/pages?new=1`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(1500);
  const createForm = await page.evaluate(() => {
    const title = document.querySelector("#page-title");
    return {
      formOpen: Boolean(title),
      focused: document.activeElement === title,
      palette: Boolean(document.querySelector("[data-search-palette]")),
      heading: (document.querySelector("h2")?.textContent || "").trim(),
    };
  });
  check("Create a page opens the form", createForm.formOpen, createForm.heading);
  check("the first field takes the focus", createForm.focused);
  check("no palette is left over the form", !createForm.palette);
  await page.locator("button:has-text('Cancel')").first().click({ timeout: 3000 }).catch(() => {});

  // Running it from the palette is the same thing end to end: the command closes the box and the
  // form it opens is ready to type in.
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  await openPalette(page);
  await page.fill("[data-palette-input]", "> create a page");
  await page.waitForTimeout(900);
  await page.locator("[data-palette-input]").press("Enter");
  await page.waitForTimeout(1700);
  const fromPalette = await page.evaluate(() => {
    const title = document.querySelector("#page-title");
    return {
      path: window.location.pathname + window.location.search,
      palette: Boolean(document.querySelector("[data-search-palette]")),
      focused: document.activeElement === title,
      formOpen: Boolean(title),
    };
  });
  check(
    "running Create a page lands on the form",
    fromPalette.formOpen && fromPalette.path.startsWith("/pages"),
    fromPalette.path,
  );
  check("the palette closed behind the command", !fromPalette.palette);
  check("and the form's first field has the focus", fromPalette.focused);
  await page.locator("button:has-text('Cancel')").first().click({ timeout: 3000 }).catch(() => {});

  // ---------------------------------------------------------------- a command run, in the history
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  await openPalette(page);
  await page.fill("[data-palette-input]", "> open media");
  await page.waitForTimeout(900);
  await page.locator("[data-palette-input]").press("Enter");
  await page.waitForTimeout(1600);
  const landed = new URL(page.url()).pathname;
  check("a command lands on its screen", landed === "/media", landed);
  check("the palette closed behind it", (await page.locator("[data-search-palette]").count()) === 0);

  const recents = await page
    .evaluate(() =>
      fetch("/api/v1/command-center/recent", { credentials: "same-origin" }).then((response) =>
        response.json(),
      ),
    )
    .catch(() => null);
  const first = recents?.items?.[0];
  check(
    "the run is in the account's own history",
    first?.kind === "command" && first?.command_id === "nav.media",
    JSON.stringify(first ?? recents),
  );
  check(
    "the history carries the command's own title and route",
    first?.title === "Open media" && first?.route === "/media",
    JSON.stringify(first ?? null),
  );

  // ---------------------------------------------------------------- clearing, immediately
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  await openPalette(page);
  await page.waitForTimeout(700);
  const recentRowsBefore = await page
    .locator('[data-search-palette] [id^="recent-"]')
    .count()
    .catch(() => 0);
  const clearButton = await page.locator("[data-palette-clear-recents]").count();
  const beforeClear = await paletteText(page);
  check("the history renders as its own group", clearButton === 1, `${clearButton} clear control(s)`);
  check("the command it ran is listed", /open media/i.test(beforeClear), `${recentRowsBefore} recent rows`);

  await page.locator("[data-palette-clear-recents]").first().click({ timeout: 3000 }).catch(() => {});
  await page.waitForTimeout(700);
  const afterClearRows = await page
    .locator('[data-search-palette] [id^="recent-"]')
    .count()
    .catch(() => 0);
  check(
    "clearing empties the group immediately",
    afterClearRows === 0 && (await page.locator("[data-palette-clear-recents]").count()) === 0,
    `${afterClearRows} rows left`,
  );

  // …and after a reload, which is what says the clear was written and not only painted.
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForTimeout(1000);
  await openPalette(page);
  await page.waitForTimeout(700);
  const afterReloadRows = await page
    .locator('[data-search-palette] [id^="recent-"]')
    .count()
    .catch(() => 0);
  const recentsApi = await page
    .evaluate(() =>
      fetch("/api/v1/command-center/recent", { credentials: "same-origin" }).then((response) =>
        response.json(),
      ),
    )
    .catch(() => null);
  check(
    "the clear survives a reload",
    afterReloadRows === 0 && Array.isArray(recentsApi?.items) && recentsApi.items.length === 0,
    `${afterReloadRows} rows · ${JSON.stringify(recentsApi?.items ?? null).slice(0, 60)}`,
  );

  // ---------------------------------------------------------------- a prefix narrows the answer
  await page.fill("[data-palette-input]", "#qa");
  await page.waitForTimeout(1300);
  const narrowed = await paletteText(page);
  const chip = (await page.locator("[data-palette-mode]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  check("the mode chip names the prefix's own kind", chip.startsWith("Sites"), chip);
  check("a narrowed answer carries its own kind", /sites/i.test(narrowed), narrowed.slice(0, 140));
  check(
    "and nothing of the other kinds",
    !/pages\b/i.test(narrowed) && !/media\b/i.test(narrowed),
    narrowed.slice(0, 140),
  );

  // ---------------------------------------------------------------- the keyboard walks the list
  await page.fill("[data-palette-input]", "qa");
  await page.waitForTimeout(1300);
  const firstRow = await activeRow(page);
  await page.locator("[data-palette-input]").press("Tab");
  await page.waitForTimeout(400);
  const secondRow = await activeRow(page);
  check("Tab moves to another section", Boolean(secondRow) && secondRow !== firstRow, `${firstRow} → ${secondRow}`);
  const groups = await page
    .locator("[data-search-palette] [role=listbox] > div")
    .count()
    .catch(() => 0);
  check("the answer has more than one group to walk", groups > 1, `${groups} groups`);
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);
  check("Escape closes the palette", (await page.locator("[data-search-palette]").count()) === 0);

  check(
    "no 5xx answers during the desktop pass",
    serverErrors.length === 0,
    serverErrors.slice(0, 3).join(" | "),
  );
  check(
    "no console errors on the desktop pass",
    consoleErrors.length === 0,
    consoleErrors.slice(0, 2).join(" | "),
  );

  // ---------------------------------------------------------------- mobile
  const mobile = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
    deviceScaleFactor: 2,
  });
  const mpage = await mobile.newPage();
  check("signed in on mobile", await signIn(mpage), mpage.url());
  await mpage.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await mpage.waitForTimeout(900);
  await mpage.click("[data-search-box]");
  await mpage.waitForTimeout(700);
  await mpage.fill("[data-palette-input]", ">").catch(() => {});
  await mpage.waitForTimeout(1000);
  const mobileRows = await mpage
    .locator('[data-search-palette] [id^="command-"]')
    .evaluateAll((rows) => rows.map((row) => Math.round(row.getBoundingClientRect().height)))
    .catch(() => []);
  check("commands are listed on a phone", mobileRows.length > 0, `${mobileRows.length} rows`);
  check(
    "phone rows are 44px",
    mobileRows.length > 0 && mobileRows.every((height) => height >= 44),
    JSON.stringify(mobileRows.slice(0, 5)),
  );

  await browser.close();

  const failed = results.filter((row) => !row.ok);
  console.log(
    `\n${results.length - failed.length}/${results.length} checks passed` +
      (failed.length ? ` — failing: ${failed.map((row) => row.name).join(", ")}` : ""),
  );
  process.exit(failed.length ? 1 : 0);
}

main().catch((error) => {
  console.error(`probe crashed: ${error.message}`);
  process.exit(1);
});
