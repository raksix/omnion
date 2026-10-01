#!/usr/bin/env node
// A targeted probe for one defect a full pass can only describe:
//
//   the panel sidebar's last navigation links are rendered, and cannot be reached.
//
// The sidebar frame is `sticky top-0 h-screen` — exactly one viewport tall — and the list inside
// it was a flex column with no `overflow`, so anything taller than the fold was clipped with no
// way to scroll to it. With ~34 entries the identity/access shelves put "Sessions", "Devices" and
// "Search settings" roughly 400px below a 900px fold: on screen, in the DOM, correct in a
// screenshot, and permanently unclickable. A pass reported it as three `click-error`s naming
// exactly those three hrefs, which is how it was found.
//
// "Rendered" and "reachable" are different properties and only a click measures the second, so
// this probe **clicks every navigation link** rather than asking whether the DOM contains it. A
// link that is present, visible by the inventory's own definition, and still unclickable is the
// whole bug — and a probe that only counted elements would have reported it green.
//
// Usage: node scripts/qa/probe-sidebar-reach.cjs [--url http://127.0.0.1:3104]
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

/** The height the pass runs at, and the one the bug was found at. */
const VIEWPORT = { width: 1440, height: 900 };

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
  const page = await browser.newPage({ viewport: VIEWPORT });
  if (!(await signIn(page))) {
    console.log("probe: could not sign in — is the QA stack running?");
    await browser.close();
    process.exit(2);
  }
  await page.goto(`${URL_ADMIN}/cdn/settings`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('[data-app-ready="1"]', { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);

  // The inventory, in the page, exactly as the pass computes it: a control is "present" when it
  // has a box and is not hidden. The bug is that a control can satisfy all of that and still not
  // be clickable, so the list below is only the starting point.
  const links = await page.evaluate(() =>
    [...document.querySelectorAll('nav[aria-label="Sections"] a[href]')]
      .map((el) => {
        const b = el.getBoundingClientRect();
        return {
          href: el.getAttribute("href"),
          label: (el.innerText || "").trim(),
          top: Math.round(b.top),
          bottom: Math.round(b.bottom),
        };
      }),
  );
  check("the nav list rendered", links.length > 0, `${links.length} links`);

  // The list must be scrollable when it is taller than the frame, and reach its own end. Asking
  // for scrollHeight vs clientHeight is what separates "there is a scrollbar" from "there is
  // content below the fold" — the second is the bug, the first is the fix.
  const scroll = await page.evaluate(() => {
    const nav = document.querySelector('nav[aria-label="Sections"]');
    if (!nav) return null;
    const style = getComputedStyle(nav);
    return {
      scrollable: /auto|scroll/.test(style.overflowY),
      scrollHeight: nav.scrollHeight,
      clientHeight: nav.clientHeight,
      overflowY: style.overflowY,
    };
  });
  check("the list can scroll", Boolean(scroll && scroll.scrollable), scroll ? `overflow-y: ${scroll.overflowY}` : "no nav");
  check(
    "the list fits or scrolls",
    Boolean(scroll && (scroll.scrollHeight <= scroll.clientHeight + 1 || scroll.scrollable)),
    scroll ? `content ${scroll.scrollHeight}px in ${scroll.clientHeight}px` : "",
  );

  // The real measurement: click every link. A link below the fold has to scroll into view and
  // land, which is what a person does and what the pass does. `scrollIntoViewIfNeeded` is not
  // allowed to stand in for the click — that would measure the API, not the product.
  const unreachable = [];
  for (const link of links) {
    const before = page.url();
    let clicked = true;
    try {
      await page.locator(`nav[aria-label="Sections"] a[href="${link.href}"]`).first().click({ timeout: 4000 });
      await page.waitForTimeout(350);
    } catch (err) {
      clicked = false;
    }
    if (!clicked) unreachable.push(`${link.label || link.href} (${link.href})`);
    if (page.url() !== before) {
      await page.goto(`${URL_ADMIN}/cdn/settings`, { waitUntil: "domcontentloaded" });
      await page.waitForSelector('[data-app-ready="1"]', { timeout: 20000 }).catch(() => {});
      await page.waitForTimeout(300);
    }
  }
  check(
    "every navigation link is clickable",
    unreachable.length === 0,
    unreachable.length ? `${unreachable.length} unreachable: ${unreachable.join(", ")}` : `${links.length} clicked`,
  );

  // The account block must stay pinned: scrolling the list must not scroll the sign-out control
  // out of the frame, which is the other half of "the frame is one viewport tall".
  const accountPinned = await page.evaluate(() => {
    const nav = document.querySelector('nav[aria-label="Sections"]');
    if (nav) nav.scrollTop = nav.scrollHeight;
    const out = [...document.querySelectorAll("button")].find((b) => /sign out/i.test(b.innerText || ""));
    if (!out) return null;
    const b = out.getBoundingClientRect();
    return { visible: b.height > 0 && b.bottom <= window.innerHeight + 1 && b.top >= 0 };
  });
  check(
    "sign out stays in the frame when the list is scrolled to its end",
    Boolean(accountPinned && accountPinned.visible),
    accountPinned ? "" : "the sign-out control left the viewport",
  );

  console.log(`\nsidebar reach: ${pass} passed, ${fail} failed`);
  await browser.close();
  process.exit(fail === 0 ? 0 : 1);
})().catch((err) => {
  console.error("probe failed:", err);
  process.exit(2);
});
