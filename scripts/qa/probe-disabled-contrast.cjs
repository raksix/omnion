#!/usr/bin/env node
/**
 * Omnion QA — focused probe: contrast of disabled filled controls.
 *
 * The walkthrough's contrast pass deliberately skips a text element that has children (an icon
 * next to a label is the common case), so a *filled* button that fades through `disabled:opacity-*`
 * is invisible to it: the label ends up alpha-composited over the page, e.g. white on a pale
 * terracotta fill. This probe measures the real rendered pair for every disabled control that has
 * a background of its own, composites the element's opacity over the ancestor background, and
 * reports the WCAG contrast ratio so the loop can prove a fix instead of waiting for a vision
 * review to repeat itself.
 *
 * Usage (same env as a pass):
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-disabled-contrast.cjs \
 *     --url http://127.0.0.1:3100 --page /ai
 *
 * Exits 1 when any measured pair is below `--min` (default 4.5).
 */
"use strict";

const { chromium } = require("playwright-core");

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i !== -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const URL_ADMIN = arg("url", "http://127.0.0.1:3100");
const PATH_PAGE = arg("page", "/ai");
const MIN = Number(arg("min", "4.5"));
const CHROME = process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const CREDS = { email: "qa-owner@omnion.test", password: "OmnionQa-Passw0rd-2026!" };

/** Runs in the page: measure every disabled control that paints a background of its own. */
function measure() {
  const parse = (value) => {
    const m = /rgba?\(([^)]+)\)/.exec(value || "");
    if (!m) return null;
    const parts = m[1].split(/[,\s/]+/).filter(Boolean).map(Number);
    return { r: parts[0], g: parts[1], b: parts[2], a: parts.length > 3 ? parts[3] : 1 };
  };
  const over = (top, bottom) => ({
    r: top.a * top.r + (1 - top.a) * bottom.r,
    g: top.a * top.g + (1 - top.a) * bottom.g,
    b: top.a * top.b + (1 - top.a) * bottom.b,
    a: 1,
  });
  const channel = (c) => {
    const v = c / 255;
    return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
  };
  const luminance = (c) => 0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b);
  const ratio = (a, b) => {
    const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
    return (hi + 0.05) / (lo + 0.05);
  };
  /** The first ancestor that paints something, composited down to the viewport. */
  const backdropOf = (el) => {
    let node = el.parentElement;
    while (node) {
      const bg = parse(getComputedStyle(node).backgroundColor);
      if (bg && bg.a > 0.99) return bg;
      node = node.parentElement;
    }
    return { r: 255, g: 255, b: 255, a: 1 };
  };

  const out = [];
  for (const el of document.querySelectorAll("button, a[role=button], input[type=submit]")) {
    if (!el.disabled) continue;
    const box = el.getBoundingClientRect();
    if (box.width < 2 || box.height < 2) continue;
    const cs = getComputedStyle(el);
    const bg = parse(cs.backgroundColor);
    if (!bg || bg.a === 0) continue; // not a filled control — nothing to measure
    const opacity = Number(cs.opacity);
    const backdrop = backdropOf(el);
    const fill = over({ ...bg, a: bg.a * opacity }, backdrop);
    const label = over({ ...parse(cs.color), a: opacity }, backdrop);
    out.push({
      label: (el.textContent || "").trim().slice(0, 40) || el.getAttribute("data-chat-send") || "?",
      hook: el.getAttribute("data-chat-send") !== null ? "data-chat-send" : null,
      opacity,
      fill: `rgb(${Math.round(fill.r)}, ${Math.round(fill.g)}, ${Math.round(fill.b)})`,
      text: `rgb(${Math.round(label.r)}, ${Math.round(label.g)}, ${Math.round(label.b)})`,
      ratio: Number(ratio(label, fill).toFixed(2)),
    });
  }
  return out;
}

(async () => {
  const browser = await chromium.launch({ executablePath: CHROME, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();

  await page.goto(`${URL_ADMIN}/login`, { waitUntil: "domcontentloaded", timeout: 30000 });

  const signIn = async (attempt) => {
    await page.fill('input[type="email"], #email', CREDS.email);
    await page.fill('input[type="password"], #password', CREDS.password);
    await page.click('button[type="submit"]');
    try {
      await page.waitForURL((url) => !/\/login/.test(url.pathname), { timeout: 20000 });
      return true;
    } catch (err) {
      // A dev-server rebuild can repaint the form mid-click; one retry is enough.
      const body = (await page.evaluate(() => document.body.innerText.slice(0, 200))).replace(/\s+/g, " ");
      console.error(`[probe] sign-in attempt ${attempt} did not leave /login: ${body}`);
      return false;
    }
  };
  if (!(await signIn(1)) && !(await signIn(2))) throw new Error("could not sign in to the QA panel");

  await page.goto(`${URL_ADMIN}${PATH_PAGE}`, { waitUntil: "domcontentloaded", timeout: 30000 });
  await page.waitForTimeout(1200);

  const measured = await page.evaluate(measure);

  // A disabled control can still match :hover in Chromium — make sure the muted fill survives it.
  const hovered = await page.evaluate(() => {
    const el = document.querySelector("[data-chat-send]");
    if (!el || !el.disabled) return null;
    const box = el.getBoundingClientRect();
    return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  });
  let hoverFill = null;
  if (hovered) {
    await page.mouse.move(hovered.x, hovered.y);
    await page.waitForTimeout(300);
    hoverFill = await page.evaluate(() => getComputedStyle(document.querySelector("[data-chat-send]")).backgroundColor);
  }

  const failures = measured.filter((m) => m.ratio < MIN);
  console.log(
    JSON.stringify(
      { page: PATH_PAGE, min: MIN, measured, hoverFill, failures: failures.length, pass: failures.length === 0 },
      null,
      1,
    ),
  );
  await browser.close();
  process.exit(failures.length === 0 ? 0 : 1);
})().catch((err) => {
  console.error("[probe] failed:", err.message);
  process.exit(2);
});
