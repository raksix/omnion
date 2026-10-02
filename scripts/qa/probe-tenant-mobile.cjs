#!/usr/bin/env node
// A targeted probe for the two REQ-005 slice-4 defects a full pass only describes:
//
//   1. the organization switcher sheet is `position: fixed; bottom: 0` in CSS, yet the pass
//      measured its bottom edge 738px above the viewport floor. A `fixed` box whose containing
//      block is not the viewport lands somewhere else entirely: an ancestor with a
//      filter/backdrop-filter/transform becomes that block. The probe walks up from the sheet and
//      names every ancestor that establishes one, so the fix targets the real cause instead of
//      nudging a `bottom` value.
//   2. every tenant depth pass skipped with "no organization to open" although the list rendered
//      one row. The probe reports what the list actually contains and which selector matches, so
//      the harness either finds the row or the panel's link is shown to be absent.
//
// Usage: node scripts/qa/probe-tenant-mobile.cjs [--url http://127.0.0.1:3104]
process.env.NODE_PATH = process.env.NODE_PATH || "/root/test-hermes/node_modules";
require("module").Module._initPaths?.();
const { chromium } = require("playwright-core");

const argUrl = (() => {
  const i = process.argv.indexOf("--url");
  return i > -1 ? process.argv[i + 1] : null;
})();
const URL_ADMIN = argUrl || process.env.QA_ADMIN_URL || "http://127.0.0.1:3104";
const CHROME = process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const EMAIL = process.env.QA_EMAIL || "qa-owner@omnion.test";
const PASSWORD = process.env.QA_PASSWORD || "OmnionQa-Passw0rd-2026!";

let pass = 0;
let fail = 0;
const check = (name, ok, detail) => {
  if (ok) {
    pass += 1;
    console.log(`  PASS ${name}${detail ? ` — ${detail}` : ""}`);
  } else {
    fail += 1;
    console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
};

async function signIn(page) {
  await page.goto(`${URL_ADMIN}/login`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(1200);
  const email = page.locator('input[type="email"]').first();
  if ((await email.count()) === 0) return false;
  await email.fill(EMAIL);
  await page.locator('input[type="password"]').first().fill(PASSWORD);
  await page.locator('button[type="submit"]').first().click();
  await page.waitForTimeout(3000);
  return !page.url().includes("/login");
}

(async () => {
  const browser = await chromium.launch({ executablePath: CHROME, args: ["--no-sandbox"] });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  if (!(await signIn(page))) {
    console.log("probe: could not sign in — is the QA stack running?");
    await browser.close();
    process.exit(2);
  }

  // ---- 1. what the organizations list actually renders -------------------------
  console.log("\n[1] organizations list");
  await page.goto(`${URL_ADMIN}/organizations`, { waitUntil: "domcontentloaded" });
  // Wait for the screen to finish loading rather than for a fixed delay. A fixed delay reads the
  // skeleton table — real `<tr>`s with no text — as "rows rendered but no links", which is a
  // product defect that does not exist. The three outcomes are: redirected to a tenant (an
  // organization account), a list with links (a platform account), or the empty state.
  const outcome = await page
    .waitForFunction(
      () => {
        const redirected = /\/organizations\/[0-9a-f-]{8}/.test(location.pathname);
        const links = document.querySelectorAll('a[href^="/organizations/"]').length;
        const empty =
          document.body.innerText.includes("No organizations yet") ||
          document.body.innerText.includes("Nothing matches that search");
        return redirected || links > 0 || empty;
      },
      { timeout: 30000 },
    )
    .then(() => "ready")
    .catch(() => "timed-out");
  await page.waitForTimeout(500);
  const list = await page.evaluate(() => {
    const rows = document.querySelectorAll("table tbody tr");
    const anchors = [...document.querySelectorAll('a[href^="/organizations/"]')];
    return {
      rowCount: rows.length,
      rowText: rows.length ? rows[0].innerText.replace(/\s+/g, " ").trim().slice(0, 90) : "",
      anchorCount: anchors.length,
      anchors: anchors.slice(0, 4).map((a) => ({ href: a.getAttribute("href"), text: a.innerText.trim().slice(0, 40) })),
      redirected: /\/organizations\/[0-9a-f-]{8}/.test(location.pathname),
      path: location.pathname,
    };
  });
  console.log("   ", JSON.stringify(list));
  check("the list screen reaches a loaded state", outcome === "ready", `outcome=${outcome}`);
  check(
    "the account is either redirected to its own tenant or offered a list",
    list.redirected || list.anchorCount > 0 || list.rowText.length > 0,
    `redirected=${list.redirected} anchors=${list.anchorCount} path=${list.path}`,
  );
  // Only a *platform* account is expected to see the list. An organization account is redirected,
  // and that is the product behaving correctly — asserting the list on that account would be
  // asserting a screen the spec deliberately does not show it.
  if (!list.redirected) {
    check("the list row links to the detail screen", list.anchorCount > 0, `${list.anchorCount} anchor(s)`);
  } else {
    console.log("   note: redirected to the account's own tenant — the list is not shown to it, by design");
  }

  // ---- 2. the sheet's containing block ---------------------------------------
  console.log("\n[2] switcher sheet geometry on a phone");
  const mobile = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  await signIn(mobile);
  await mobile.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" });
  await mobile.waitForTimeout(1800);
  await mobile.locator('button[aria-label="Current organization"]').first().click({ timeout: 8000 }).catch(() => {});
  await mobile.waitForTimeout(800);
  const sheet = await mobile
    .evaluate(() => {
      const dialog = document.querySelector("[data-org-switcher]");
      if (!dialog) return { missing: true };
      const rect = dialog.getBoundingClientRect();
      // Every ancestor that would make this `fixed` box resolve against something other than
      // the viewport. `filter` is non-none for `backdrop-filter` too, and both create a
      // containing block for fixed descendants — that is the whole question.
      const blockers = [];
      for (let el = dialog.parentElement; el; el = el.parentElement) {
        const cs = getComputedStyle(el);
        const makes = [];
        if (cs.position === "fixed" || cs.position === "sticky") makes.push(`position:${cs.position}`);
        if (cs.filter && cs.filter !== "none") makes.push(`filter:${cs.filter}`);
        if (cs.backdropFilter && cs.backdropFilter !== "none") makes.push(`backdrop-filter:${cs.backdropFilter}`);
        if (cs.transform && cs.transform !== "none") makes.push("transform");
        if (cs.perspective && cs.perspective !== "none") makes.push("perspective");
        if (cs.contain && /paint|layout|strict|content/.test(cs.contain)) makes.push(`contain:${cs.contain}`);
        if (cs.willChange && /transform|filter|perspective/.test(cs.willChange)) makes.push(`will-change:${cs.willChange}`);
        if (cs.containerType && cs.containerType !== "normal") makes.push(`container-type:${cs.containerType}`);
        if (makes.length) {
          blockers.push({
            tag: el.tagName.toLowerCase(),
            cls: String(el.className || "").slice(0, 90),
            makes,
          });
        }
      }
      return {
        missing: false,
        position: getComputedStyle(dialog).position,
        bottom: Math.round(rect.bottom),
        top: Math.round(rect.top),
        viewport: { w: innerWidth, h: innerHeight },
        gapToBottom: Math.round(innerHeight - rect.bottom),
        blockers,
      };
    })
    .catch(() => ({ missing: true, error: String(new Error("evaluate failed")) }));
  console.log("   ", JSON.stringify(sheet, null, 1).split("\n").join("\n    "));
  check("the sheet exists on a phone", !sheet.missing);
  if (!sheet.missing) {
    check(
      "no ancestor captures the fixed sheet",
      sheet.blockers.length === 0,
      sheet.blockers.map((b) => `${b.tag} [${b.makes.join(",")}]`).join(" | ") || "none",
    );
    check(
      "the sheet reaches the bottom edge",
      sheet.gapToBottom <= 2,
      `${sheet.gapToBottom}px above the floor in a ${sheet.viewport.h}px viewport`,
    );
  }

  await browser.close();
  console.log(`\nprobe: ${pass} passed, ${fail} failed`);
  process.exit(fail > 0 ? 1 : 0);
})().catch((error) => {
  console.error("probe failed:", error);
  process.exit(3);
});
