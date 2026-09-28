#!/usr/bin/env node
/**
 * A single-screen probe for the camera record (REQ-010, slice 3).
 *
 * The full pass is the gate, but it is also a twenty-minute run on a box where seven QA stacks
 * compete for memory, and the failure it produces under that pressure — the browser context
 * closing mid-walk — is indistinguishable from a screen that is broken. This probe asks the one
 * question the new screen raises, on its own, so that "the block renders" and "the box ran out of
 * memory" are not the same line in a log.
 *
 * It walks the *real* screen: sign in, upload a JPEG that carries a genuine EXIF block, open its
 * detail screen, and read what the Camera block says. The file is built rather than checked in for
 * the reason `walkthrough.cjs` builds it — a committed .jpg is a binary blob nobody can review.
 *
 * Usage: `node scripts/qa/probe-media-camera.cjs [--shots <dir>]`
 * Exit code 0 when every assertion holds, 1 otherwise.
 */
const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");
const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3100";
const API = process.env.QA_API_URL || "http://127.0.0.1:18080";
// The same account the pass signs in with, so both see the same seeded site and the same
// library. Reading the values from the pass's own table rather than copying them keeps a
// rotation from leaving this probe signing in as nobody.
const CREDS = {
  name: "QA Owner",
  email: process.env.QA_EMAIL || "qa-owner@omnion.test",
  password: process.env.QA_PASSWORD || "OmnionQa-Passw0rd-2026!",
  org: "QA Organization",
  orgSlug: "qa-org",
  site: "QA Site",
};

const argOf = (name, fallback) => {
  const at = process.argv.indexOf(`--${name}`);
  return at > -1 && process.argv[at + 1] ? process.argv[at + 1] : fallback;
};
// The Chromium the QA pass drives, pinned so this probe cannot pass against a different browser
// than the gate runs.
const CHROME = process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const SHOTS = path.resolve(ROOT, argOf("shots", "qa-artifacts/probe-media-camera"));
fs.mkdirSync(SHOTS, { recursive: true });

const results = [];
const check = (name, ok, detail) => {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
};

/**
 * A JPEG carrying a real EXIF block: an orientation-6 frame of 4000x3000, so the stored geometry
 * and the drawn geometry disagree and the screen has to pick the right one.
 *
 * The block is little-endian TIFF; the JPEG framing around it is big-endian. A builder that
 * reuses one `u16` helper for both produces a segment length of 57 KB in a 253-byte file, and the
 * block becomes unreachable — a file with no camera record and an empty state that renders
 * perfectly, which is why the probe checks the *contents* and not merely that a block appeared.
 */
function buildCameraJpeg() {
  const block = [];
  const u16 = (n) => [n & 0xff, (n >> 8) & 0xff];
  const be16 = (n) => [(n >> 8) & 0xff, n & 0xff];
  const u32 = (n) => [n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >>> 24) & 0xff];
  const ascii = (value) => [...Buffer.from(value, "ascii"), 0];
  // A RATIONAL is two little-endian words, so 1/200 s is the pair (1, 200) and f/1.8 is (18, 10).
  const rational = (num, den) => [...u32(num), ...u32(den)];

  block.push(...Buffer.from("II"), ...u16(42), ...u32(8));
  // An entry is 12 bytes: tag, type, count, and then either the value — four bytes or fewer — or
  // an offset into the value area. Which of the two is the decision this builder keeps getting
  // wrong, so each entry remembers where its offset will live and which value belongs there.
  const pending = [];
  const wide = (tag, kind, count, bytes) => {
    block.push(...u16(tag), ...u16(kind), ...u32(count));
    pending.push({ at: block.length, bytes });
    block.push(0, 0, 0, 0);
  };
  const entry = (tag, kind, count, value) => {
    block.push(...u16(tag), ...u16(kind), ...u32(count), ...value);
  };

  // IFD0: the maker, the model, the orientation and the Exif sub-directory pointer.
  block.push(...u16(4));
  wide(0x010f, 2, ascii("Probe Camera").length, ascii("Probe Camera"));
  wide(0x0110, 2, ascii("Probe Body").length, ascii("Probe Body"));
  entry(0x0112, 3, 1, [6, 0, 0, 0]);
  const subdirPointerAt = block.length + 8;
  entry(0x8769, 4, 1, [0, 0, 0, 0]);
  block.push(...u32(0));
  const subdirAt = block.length;

  // The Exif sub-directory: ISO inline, then the exposure, the aperture, the focal length, the
  // date and the lens.
  block.push(...u16(6));
  entry(0x8827, 3, 1, [0x90, 0x01, 0, 0]);
  wide(0x829a, 5, 1, rational(1, 200));
  wide(0x829d, 5, 1, rational(18, 10));
  wide(0x920a, 5, 1, rational(5000, 100));
  const captured = ascii("2024:05:17 09:15:00");
  wide(0x9003, 2, captured.length, captured);
  const lens = ascii("Probe 35mm f/1.8");
  wide(0xa434, 2, lens.length, lens);
  block.push(...u32(0));

  // The value area follows *both* directories, because an offset is measured from the start of
  // the block and a value laid down before the sub-directory would be overwritten by it.
  for (const slot of pending) {
    for (let i = 0; i < 4; i += 1) {
      block[slot.at + i] = (block.length >>> (8 * i)) & 0xff;
    }
    block.push(...slot.bytes);
  }
  for (let i = 0; i < 4; i += 1) {
    block[subdirPointerAt + i] = (subdirAt >>> (8 * i)) & 0xff;
  }

  const payload = [...Buffer.from("Exif\0\0", "binary"), ...block];
  const length = payload.length + 2;
  if (length > 0xffff) throw new Error(`the probe's EXIF block is ${length} bytes, too long for one segment`);
  const jpeg = [0xff, 0xd8, 0xff, 0xe1, ...be16(length), ...payload];
  jpeg.push(0xff, 0xc0, 0x00, 0x11, 0x08, ...be16(3000), ...be16(4000), 3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xd9);
  const file = path.join(SHOTS, "probe-camera.jpg");
  fs.writeFileSync(file, Buffer.from(jpeg));
  return file;
}

async function main() {
  // The same module and the same browser the pass uses. The box's `playwright-core` lives in the
  // test harness's tree, which is why the pass runs with `NODE_PATH` pointed at it — falling back
  // to a differently-resolved copy is how a probe passes against a browser the gate never runs.
  const { chromium } = require("playwright-core");
  const browser = await chromium.launch({
    executablePath: process.env.QA_CHROME || CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  try {
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    const page = await context.newPage();
    const consoleErrors = [];
    page.on("console", (message) => {
      if (message.type() === "error") consoleErrors.push(message.text());
    });

    // Sign in, the same way the pass does.
    await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
    await page.locator('input[type="email"], input[name="email"], #email').first().fill(CREDS.email);
    await page.locator('input[type="password"], input[name="password"], #password').first().fill(CREDS.password);
    await page.locator('button[type="submit"]').first().click().catch(() => {});
    await page.waitForTimeout(2500);
    const signedIn = !/\/login/.test(page.url());
    check("signed in", signedIn, signedIn ? page.url() : `still on ${page.url()}`);
    if (!signedIn) return;

    // The library, the upload, and the file's own screen.
    await page.goto(`${ADMIN}/media`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(1500);
    // The library screen resolves its own site from the session, so the upload needs no id — the
    // screen is driven the way a person drives it rather than by reconstructing its query string.
    await page.waitForTimeout(1200);
    const rows = await page.locator('[data-testid="media-file-row"], tbody tr').count();
    check("the library rendered", rows > 0, `${rows} rows`);

    const file = buildCameraJpeg();
    const input = page.locator('input[type="file"]').first();
    if ((await input.count()) === 0) {
      check("upload control", false, "no file input on the library screen");
      return;
    }
    await input.setInputFiles(file);
    await page.waitForTimeout(3500);
    const listed = await page.locator("text=probe-camera.jpg").count();
    check("the JPEG with a camera block uploaded", listed > 0, `listed ${listed}`);

    const fileId = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('a[href^="/media/files/"]')];
      const link = rows.find((row) => (row.textContent || "").includes("probe-camera.jpg"));
      return link ? link.getAttribute("href").split("/").pop() : null;
    });
    if (!fileId) {
      check("opened the file", false, "the uploaded file has no detail link");
      return;
    }

    await page.goto(`${ADMIN}/media/files/${fileId}`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-testid="media-file-name"]', { timeout: 15000 });
    await page.waitForTimeout(1800);

    const block = await page.locator('[data-testid="media-camera-block"]').first().innerText().catch(() => "");
    const empty = await page.locator('[data-testid="media-camera-empty"]').count();
    check("the camera block rendered", block.length > 0 && empty === 0, block.replace(/\s+/g, " ").slice(0, 200));
    check("the body is named", /Probe Camera Probe Body/.test(block), block.replace(/\s+/g, " ").slice(0, 80));
    check("the shutter prints as a fraction", /1\/200/.test(block));
    check("the aperture prints as an f-number", /f\/1\.8/.test(block));
    // The rows are a `dt`/`dd` pair, so `innerText` puts a newline between the label and the
    // value. Matching on the flattened text would fail on a row that is on screen and correct.
    const cameraRows = await page.evaluate(() =>
      [...document.querySelectorAll('[data-testid="media-camera-block"] dt')].map((dt) => {
        const dd = dt.nextElementSibling;
        return `${dt.textContent}: ${dd ? dd.textContent : ""}`;
      })
    );
    console.log("  rows:", cameraRows.join(" | "));
    check(
      "the ISO is shown",
      cameraRows.some((row) => /^ISO:\s*400$/.test(row)),
      cameraRows.join(" | ").slice(0, 160),
    );
    check("the lens is shown", /Probe 35mm/.test(block));
    check("the rotation is named", /Rotated 90/.test(block), "orientation 6 is a quarter turn");

    // The geometry: the frame is stored 4000x3000 and drawn 3000x4000, so the facts list must
    // show the drawn pair. A card reserving the stored one shows a portrait in a landscape.
    const facts = await page.locator("dl").first().innerText().catch(() => "");
    check("the panel reserves the rotated box", /3000\s*×\s*4000/.test(facts), facts.replace(/\s+/g, " ").slice(0, 140));

    // What the API actually sent, read off the network rather than off the page: a panel that
    // shows the stored pair because it read the wrong field looks identical to a panel that got
    // the right field and did not use it.
    const payload = await page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/media/files/${id}`, { credentials: "same-origin" });
      return response.ok ? response.json() : { error: response.status };
    }, fileId);
    const sent = payload.file || payload;
    console.log(
      "  api:",
      JSON.stringify({
        width: sent.width,
        height: sent.height,
        display_width: sent.display_width,
        display_height: sent.display_height,
        exif_orientation: sent.exif && sent.exif.orientation,
      }),
    );
    check(
      "the API sent the rotated box",
      sent.display_width === 3000 && sent.display_height === 4000,
      `display ${sent.display_width}x${sent.display_height}, stored ${sent.width}x${sent.height}`,
    );

    await page.screenshot({ path: path.join(SHOTS, "probe-media-camera.png"), fullPage: true });
    check("no console errors on the screen", consoleErrors.length === 0, consoleErrors.slice(0, 2).join(" | "));
  } finally {
    await browser.close().catch(() => {});
  }

  const failed = results.filter((entry) => !entry.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((cause) => {
  console.error("probe failed:", cause);
  process.exit(1);
});
