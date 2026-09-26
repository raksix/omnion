#!/usr/bin/env node
/**
 * Omnion QA — focused probe for the palette's action commands (REQ-032, slice 3).
 *
 * The walkthrough opens the palette, narrows it and runs a navigation command; this probe proves
 * what a click-through cannot see on its own: that an action command asks before it runs and
 * really runs nothing until the caller answers, that the answer the owning service gives is what
 * the card prints, that exactly one `command.run` audit entry lands per executed command (read
 * back through the audit API, not the DOM), and that the endpoint refuses an unconfirmed run, a
 * navigation command and an unknown id with their own codes.
 *
 * Usage (the QA stack has to be up — `bash scripts/qa/run.sh` leaves it running):
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-command-actions.cjs
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

/**
 * The `command.run` entries of the audit trail, newest first — the slice's own acceptance
 * criterion, read from the API the operator would read.
 */
async function auditRuns(page) {
  const body = await page
    .evaluate(() =>
      fetch("/api/v1/iam/audit?limit=50", { credentials: "same-origin" }).then((response) =>
        response.ok ? response.json() : { entries: [] },
      ),
    )
    .catch(() => null);
  const entries = Array.isArray(body?.entries) ? body.entries : [];
  return entries.filter((entry) => entry.action === "command.run");
}

/** POST one command's run endpoint from the page, with cookies attached. */
async function runFromPage(page, id, body) {
  return page.evaluate(
    async ([commandId, payload]) => {
      const response = await fetch(`/api/v1/commands/${commandId}/run`, {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(payload),
      });
      let parsed = null;
      try {
        parsed = await response.json();
      } catch {
        parsed = null;
      }
      return { status: response.status, body: parsed };
    },
    [id, body],
  );
}

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

  // ---------------------------------------------------------------- the action rows say what they are
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  check("the palette opens", await openPalette(page));
  await page.fill("[data-palette-input]", "> rebuild");
  await page.waitForTimeout(1000);

  const actionBadges = await page.locator('[data-palette-command-kind="action"]').count();
  const rowText = await paletteText(page);
  check("the action command is offered to its holder", /rebuild the search index/i.test(rowText), rowText.slice(0, 120));
  check("and its row carries an Action badge", actionBadges >= 1, `${actionBadges} badge(s)`);
  check("the row says it asks first", /asks first/i.test(rowText), rowText.slice(0, 160));

  // ---------------------------------------------------------------- nothing runs before the yes
  const before = await auditRuns(page);
  await page.locator("[data-palette-input]").press("Enter");
  await page.waitForTimeout(700);

  const confirmCard = await page.locator("[data-palette-confirm]").first();
  const confirmVisible = (await page.locator("[data-palette-confirm]").count()) === 1;
  const confirmText = confirmVisible
    ? (await confirmCard.innerText()).replace(/\s+/g, " ").trim()
    : "";
  check("activating it asks before it runs", confirmVisible, confirmText.slice(0, 140));
  check(
    "the question names the command and its cost",
    /rebuild the search index/i.test(confirmText) && /cannot be undone/i.test(confirmText),
    confirmText.slice(0, 160),
  );
  const afterPrompt = await auditRuns(page);
  check(
    "asking runs nothing",
    afterPrompt.length === before.length,
    `${before.length} → ${afterPrompt.length} command.run entries`,
  );
  check(
    "no result card is painted before the answer",
    (await page.locator("[data-palette-run-result]").count()) === 0,
  );

  // ---------------------------------------------------------------- Escape takes the question back
  await page.locator("[data-palette-input]").press("Escape");
  await page.waitForTimeout(500);
  check(
    "Escape withdraws the question and leaves the palette open",
    (await page.locator("[data-palette-confirm]").count()) === 0 &&
      (await page.locator("[data-search-palette]").count()) === 1,
  );
  const afterCancel = await auditRuns(page);
  check(
    "a withdrawn question runs nothing either",
    afterCancel.length === before.length,
    `${afterCancel.length} command.run entries`,
  );

  // ---------------------------------------------------------------- the yes runs it once, for real
  await page.locator("[data-palette-input]").press("Enter");
  await page.waitForTimeout(600);
  check(
    "the question can be opened again",
    (await page.locator("[data-palette-confirm]").count()) === 1,
  );
  await page.locator("[data-palette-confirm-run]").first().click();
  await page.waitForTimeout(3500);

  const runCard = page.locator('[data-palette-run-result="done"]');
  const doneVisible = (await runCard.count()) === 1;
  const doneText = doneVisible ? (await runCard.innerText()).replace(/\s+/g, " ").trim() : "";
  check("running it answers in the palette", doneVisible, doneText.slice(0, 160));
  check(
    "the line is the owning service's own answer",
    /rebuilt/i.test(doneText) && /documents/i.test(doneText),
    doneText.slice(0, 200),
  );
  check(
    "and the card offers the screen that reads the record back",
    (await page.locator("[data-palette-run-details]").count()) === 1,
  );

  const afterRun = await auditRuns(page);
  const newRuns = afterRun.slice(0, afterRun.length - before.length);
  check(
    "exactly one audit entry per executed command",
    afterRun.length === before.length + 1,
    `${before.length} → ${afterRun.length} command.run entries`,
  );
  check(
    "the entry names the command as its target",
    newRuns[0]?.target_type === "command" && newRuns[0]?.target_id === "act.reindex-search",
    JSON.stringify(newRuns[0] ?? null).slice(0, 160),
  );
  check(
    "the entry carries the actor and the outcome",
    Boolean(newRuns[0]?.actor_user_id) && newRuns[0]?.metadata?.outcome === "ok",
    JSON.stringify(newRuns[0]?.metadata ?? null).slice(0, 120),
  );

  // ---------------------------------------------------------------- the endpoint's own refusals
  const unconfirmed = await runFromPage(page, "act.reindex-search", {});
  check(
    "the API refuses an unconfirmed run",
    unconfirmed.status === 400 && unconfirmed.body?.error?.code === "confirmation_required",
    `${unconfirmed.status} ${unconfirmed.body?.error?.code ?? ""}`,
  );
  const navigational = await runFromPage(page, "nav.pages", { confirm: true });
  check(
    "the API refuses to run a command that opens a screen",
    navigational.status === 400 && navigational.body?.error?.code === "not_runnable",
    `${navigational.status} ${navigational.body?.error?.code ?? ""}`,
  );
  const unknown = await runFromPage(page, "act.nope", { confirm: true });
  check(
    "an unknown command is a 404, not a crash",
    unknown.status === 404 && unknown.body?.error?.code === "unknown_command",
    `${unknown.status} ${unknown.body?.error?.code ?? ""}`,
  );
  const afterRefusals = await auditRuns(page);
  check(
    "a refused run is not an executed one",
    afterRefusals.length === afterRun.length,
    `${afterRefusals.length} command.run entries`,
  );

  // ---------------------------------------------------------------- the second action really acts
  await page.fill("[data-palette-input]", "> clear");
  await page.waitForTimeout(900);
  const clearOffered = /clear command history/i.test(await paletteText(page));
  check("the second action is offered too", clearOffered, (await paletteText(page)).slice(0, 140));
  await page.locator("[data-palette-input]").press("Enter");
  await page.waitForTimeout(600);
  await page.locator("[data-palette-confirm-run]").first().click();
  await page.waitForTimeout(2500);

  const clearCard = await page
    .locator('[data-palette-run-result="done"]')
    .first()
    .innerText()
    .catch(() => "");
  check(
    "clearing answers with what it forgot",
    /cleared the palette history/i.test(clearCard.replace(/\s+/g, " ")),
    clearCard.replace(/\s+/g, " ").slice(0, 160),
  );
  const afterClear = await auditRuns(page);
  check(
    "the clear left its own audit entry",
    afterClear.length === afterRun.length + 1 &&
      afterClear[0]?.target_id === "act.clear-recents",
    JSON.stringify(afterClear[0]?.target_id ?? null),
  );

  // …and the history really is gone: the group does not come back after a reload.
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForTimeout(1000);
  await openPalette(page);
  await page.waitForTimeout(900);
  const recentRowsAfter = await page
    .locator('[data-search-palette] [id^="recent-"]')
    .count()
    .catch(() => 0);
  check("the cleared history stays cleared", recentRowsAfter === 0, `${recentRowsAfter} rows`);

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

  // ---------------------------------------------------------------- mobile: the card is usable
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
  await mpage.fill("[data-palette-input]", "> rebuild").catch(() => {});
  await mpage.waitForTimeout(1000);
  await mpage.locator("[data-palette-input]").press("Enter");
  await mpage.waitForTimeout(700);
  const mobileCard = await mpage.locator("[data-palette-confirm]").count();
  const mobileTargets = await mpage
    .locator("[data-palette-confirm-run], [data-palette-confirm-cancel]")
    .evaluateAll((buttons) => buttons.map((button) => Math.round(button.getBoundingClientRect().height)))
    .catch(() => []);
  check("the question shows on a phone", mobileCard === 1, `${mobileCard} card(s)`);
  check(
    "its answers are 44px tall on a phone",
    mobileTargets.length === 2 && mobileTargets.every((height) => height >= 44),
    JSON.stringify(mobileTargets),
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
