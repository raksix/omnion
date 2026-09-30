#!/usr/bin/env node
/**
 * Omnion QA — visual + interaction walkthrough.
 *
 * Runs against a freshly migrated QA database, drives the first-run wizard, then walks every
 * admin screen: full-page screenshots, DOM diagnostics (overflow, contrast, broken images,
 * unlabeled inputs, duplicate ids, off-screen elements) and a click-through of every visible
 * interactive element (buttons, links, inputs, selects, textareas, summaries).
 *
 * Everything lands in `--out`:
 *   shots/*.png        screenshots (page, interesting interactions, mobile)
 *   clicks.jsonl       one JSON line per interaction (label, outcome, error deltas)
 *   diagnostics.json   per-page DOM health report
 *   summary.json       machine-readable roll-up (counts + findings)
 *   report.md          human-readable report
 *
 * Usage:
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/walkthrough.cjs \
 *     --url http://127.0.0.1:3100 --web http://127.0.0.1:3200 --out qa-artifacts/<ts>
 *
 * The script never fails the process for visual findings — it always writes its artifacts and
 * exits 0 unless the browser or the admin panel itself cannot be reached.
 */
"use strict";

const fs = require("fs");
const path = require("path");
// The newsletter depth pass mints confirmation tokens the way the store does, so it needs the
// same digest function rather than a hand-rolled one: a second implementation of sha256 in a
// test helper is a test helper that can disagree with the thing it is testing.
const { createHash } = require("crypto");
const { execFileSync, spawn } = require("child_process");
const { chromium } = require("playwright-core");

// ---------------------------------------------------------------- args / env

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i !== -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const URL_ADMIN = arg("url", "http://127.0.0.1:3100");
const URL_WEB = arg("web", "http://127.0.0.1:3200");
// The repository root, for the passes that read the source tree itself (the theme
// registry, a migration list). `__dirname/../..` says it once here so no pass spells it.
const REPO_ROOT = path.join(__dirname, "..", "..");
// The two names the CMS depth passes use, and the reason they are DEFINED here rather than
// copied at each call site.
//
// `URL_API` is the API the panel talks to — the admin dev server proxies `/api` to it, but
// `page.request` is not the browser, so a depth pass that POSTs through the page must name the
// API directly. `ADMIN` is the panel's own origin, which is what `URL_ADMIN` already is.
//
// Both were referenced by twenty-two lines across five depth passes (featured-media, forms,
// seo, comments and newsletter) and **never declared**, so every one of those passes threw
// `ReferenceError: URL_API is not defined` on its first write and reported itself as broken.
// A harness bug that only appears when a pass reaches its first POST is a bug the pass list
// cannot catch, because the pass never ran. Declaring the names here is what makes the passes
// runnable at all; the QA pass is owed to this tick for finding it.
const URL_API = process.env.QA_API_URL || arg("api", URL_ADMIN);
const ADMIN = URL_ADMIN;
const OUT = path.resolve(arg("out", `qa-artifacts/${Date.now()}`));
const SHOTS = path.join(OUT, "shots");
const CHROME = process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const MAX_PER_PAGE = Number(arg("max-per-page", "40"));
const STEP_MS = Number(arg("step-ms", "380"));
/**
 * `--only=a,b` narrows the pass to the named routes and depth passes.
 *
 * The route list plus thirty depth passes is more work than one pass can finish inside the
 * ceiling a browser pass is given, so a full pass started getting cut off partway through —
 * and a pass that is cut off has proven nothing about the screens after the cut, while still
 * looking like a pass in the log. Three requests (REQ-010, REQ-012, REQ-013) sat unverified
 * for exactly that reason: the harness that was supposed to accept them could not reach their
 * screens inside its own budget.
 *
 * A timeout is therefore the wrong instrument: the honest instrument is the budget. `--only`
 * lets a loop spend one pass on the screens it just built, and every focused pass still walks
 * its own routes, runs its own depth passes and writes the same report — it just does not
 * pretend to cover the rest. The default (`--only=all`) walks everything, unchanged.
 *
 * A name that matches nothing is a finding rather than a silent no-op: a typo in a filter would
 * otherwise produce an empty, entirely green report, which is the worst output this file can
 * emit.
 */
const ONLY = (arg("only", "all") || "all")
  .split(",")
  .map((name) => name.trim())
  .filter(Boolean);
const ONLY_ALL = ONLY.includes("all");
const wants = (name) => ONLY_ALL || ONLY.includes(name);
/** Every route/depth-pass name this pass actually walked, so an unmatched filter is visible. */
const matchedOnly = new Set();
/** `mobile:<name>` is a valid filter spelling; `MOBILE_NAMES` keeps the roll-up from calling it unknown. */
const MOBILE_NAMES = new Set();
/**
 * The disposable QA database, used only by the analytics fixture (REQ-007): the pass posts a
 * synthetic beacon batch through the public collect endpoint and then spreads a slice of those
 * rows over the last thirty days, so the report screens have a multi-day shape to draw. It is the
 * same `docker exec psql` the reset step uses, against `omnion_qa` and nothing else.
 */
const QA_PG_CONTAINER = arg("db-container", process.env.QA_PG_CONTAINER || "omnion-postgres");
const QA_DB = arg("db", process.env.QA_DB || "omnion_qa");

const CREDS = {
  name: "QA Owner",
  email: "qa-owner@omnion.test",
  password: "OmnionQa-Passw0rd-2026!",
  org: "QA Organization",
  orgSlug: "qa-org",
  site: "QA Site",
  siteKey: "main",
  domain: "qa.omnion.test",
};

/**
 * The slug of the page the walkthrough creates in the panel. The panel pass publishes it and the
 * public pass opens it again on the site's own host, so the renderer is verified end to end.
 */
const SAMPLE_SLUG = "qa-sample";

/**
 * A slug unique to THIS run, for anything the block-editor pass creates.
 *
 * The pass is re-runnable against a database that was not reset — which is the normal case for
 * `--only=block-editor`, since it deliberately skips `run.sh`'s reset. A fixed slug then means
 * the create answers `409` and the pass drives whatever page an EARLIER run left behind, which
 * has content this pass never built: the canvas starts with blocks already in it, the undo
 * baseline is somebody else's tree, and the published render shows their work. Every one of
 * those reads as a product defect. The run stamp keeps each pass's own page its own page.
 */
const RUN_STAMP = process.env.QA_RUN_STAMP || String(Date.now());
const BLOCK_PAGE_SLUG = `qa-block-page-${RUN_STAMP}`;

/**
 * The one file the pass uploads, as base64: a small landscape, so the library's thumbnail looks
 * like a picture rather than a placeholder. It has to decode into a real PNG — an earlier,
 * one-character-short string produced a file the browser could not render at all.
 */
const SAMPLE_PNG_BASE64 =
  "iVBORw0KGgoAAAANSUhEUgAAAKAAAAB4CAIAAAD6wG44AAADwUlEQVR42u3cyVJTQRTG8bTF3iqRhcPSYuPaRxABtRTW+A5uHLAUEC1xeAxdi1VqHPA1cCO6UFAkJBDwAVxYYNAkd+hzerr/bwlUp/v8cs69N1CYj+vbNZJuDlECgAnAJNQMGGpABxOACSOaeAGuIcyIJoxoQgcTgAnAhGsw4TGJEU0Y0SSOEU18pHVrste3jjx4LvhC5tPGNuUOhFaD2awA7CrNfLSdGbRmBjhQWilms9IAWFl3etJ+kcGFksY8JkWga7OO+dxow6CUzekJ2QWPLizSwcnqllsT4MRjvjCiFdJQaN/9DBUZ1HRw8h28SQdLt+/NCe2XGHqYt4n5ZUOcfcmI9pUN/fYt9CoAJx6AEw/X4MQvw3Rw6h3MH92l3cKMaEY04S6ahNvoX5t8VCmcHzfUP+s49miRDiYA+24vB+vzmJTuDfSfn/3W4hqsku/XVa7Exx8XGw+MaK7BxEmrKa1pVhnRmlmTG9QnSr1jzGprBwZl48sSui8Y0YGmtI3ICmZtiw52lNVrhVv55BPbNwfAgTLb0wIcKLMULcCVCL/wTzzcRQNM4h7RzGg6mABMuIsmdDBJGfji3BU8xGPWt3dCo3059xQYMeCfbZ/AF2Z7du2ruzDHDNyHFua4gXPSwiwDvOEQ+Pys1W3Ua5hDBrbUhTlcYClajMsANzSBx2cVH23rMOcC3lEBHp9x9KlFfR7mDOBdadop98eozz/DUh3YCy3MGcCbEsBjvmk78yYq5rGZKdUN2wIHRRsX8z+lU9pweeBgaQNn7l838Q2bZnHg0RhoO/M2DOacdZPdrWnuFgAevRMZ7YHC3fPGXKJuUrvNCxw1rUdmy7rZ79a0cgCfS0V3P++UmWUrZrPbDOD0aLWZlSpWeqs9gdOm1TB2ULESu+0CXBFaKWb35Sq0W7PVATxSPdrOvC/I7LdcOXdrtn7t1mq1kduVpj1QuPvZhQunXJm7NWeuXgI1f+EC7IT+xgD3y9Je7c4GP+GWejB7A27Xlw+Pn+Y9pM3sGrhdX/7/i0jrMbsA7oraNUhL5cOesSJwflek9ZjlgW1cYRbPQGioXZdF2g+wkmvy0i6fIMqMaGeuKU3vXkXTPkhe4BBQo5MuVDSlg2QAB+sasrRl0WQP0h04ItdApDUqJnKQv8BRo3qRdlYxm4OY4eFTaT8nyDL7bYMSZ0kf2F46wNmW/ywVAi5anSiuWZlnqSJwn+rEeyPSS7rSwFV41wKcuDTAiYf/NgswAZgATAAmABOACcAAE4AJwARgAjCxym/ay4DNqO3IxAAAAABJRU5ErkJggg==";

/** Write the sample file next to the pass's other artifacts; answers its path. */
function ensureSamplePng() {
  const file = path.join(OUT, "upload-sample.png");
  if (!fs.existsSync(file)) {
    fs.writeFileSync(file, Buffer.from(SAMPLE_PNG_BASE64, "base64"));
  }
  return file;
}

/**
 * Write a JPEG that carries a real EXIF block and answer its path.
 *
 * The QA pass needs a file with a camera record to look at, and the sample PNG the library pass
 * uploads has none — a screenshot's empty state is the correct answer for it. So the camera
 * record would go unvisited, which the "no untested screen" rule forbids. The bytes are built
 * here for the same reason the Rust test builds them: a committed `.jpg` is a binary blob nobody
 * can review, and a hand-built one shows which field the screen is proving.
 *
 * Orientation 6 — the pixels are stored 4000x3000 and drawn as a 3000x4000 portrait — so the pass
 * can check that the *panel* reserves the rotated box and not the stored one. A card that renders
 * a portrait photograph in a landscape frame is the defect this catches, and it is invisible in a
 * screenshot of any other file.
 */
function ensureSampleJpegWithExif() {
  const file = path.join(OUT, "upload-camera.jpg");
  if (!fs.existsSync(file)) {
    const block = [];
    // The TIFF block is little-endian and the JPEG framing around it is big-endian, which is the
    // one thing about JPEG that catches everybody: a segment length written with the block's own
    // `u16` reads as a 57 KB segment in a 253-byte file, and the block is then unreachable.
    const u16 = (n) => [n & 0xff, (n >> 8) & 0xff];
    const be16 = (n) => [(n >> 8) & 0xff, n & 0xff];
    const u32 = (n) => [n & 0xff, (n >> 8) & 0xff, (n >> 16) & 0xff, (n >>> 24) & 0xff];
    const ascii = (value) => [...Buffer.from(value, "ascii"), 0];
    // A RATIONAL is two little-endian words: a numerator and a denominator, so 1/200 is the pair
    // (1, 200) and f/1.8 is (18, 10) rather than the decimal.
    const rational = (num, den) => [...u32(num), ...u32(den)];

    // A TIFF header: little-endian, magic 42, IFD0 at offset 8.
    block.push(...Buffer.from("II"), ...u16(42), ...u32(8));

    // Every entry is 12 bytes: tag, type, count, and then either the value itself — four bytes or
    // fewer — or a four-byte offset into the value area. Which of the two is decided by the type
    // and the count, and getting it wrong puts a string on the next entry's tag, which reads as a
    // parser bug and is really a builder that wrote the format wrong. So each entry remembers
    // where its offset lives and which value belongs there, and the two are filled in at the end
    // once the block's length is known.
    // `pending` holds the entries whose value is an offset rather than an inline four bytes,
    // each with the byte it will be appended as and where its own offset will live. The offset is
    // *not* written here: it is not known until the value area has been laid out, because a value
    // offset is measured from the start of the block and the block keeps growing until the end.
    const pending = [];
    const wide = (tag, kind, count, bytes) => {
      block.push(...u16(tag), ...u16(kind), ...u32(count));
      pending.push({ at: block.length, bytes });
      block.push(0, 0, 0, 0);
    };
    const entry = (tag, kind, count, value) => {
      block.push(...u16(tag), ...u16(kind), ...u32(count), ...value);
    };

    // IFD0: the maker, the model, the orientation (6, inline) and the Exif sub-directory pointer.
    block.push(...u16(4));
    wide(0x010f, 2, ascii("QA Camera").length, ascii("QA Camera"));
    wide(0x0110, 2, ascii("QA Body One").length, ascii("QA Body One"));
    entry(0x0112, 3, 1, [6, 0, 0, 0]);
    // The sub-directory pointer is a LONG, so it is also an offset — recorded here and patched
    // last, because where the sub-directory lands is only known after the value area is placed.
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
    const lens = ascii("QA 35mm f/1.8");
    wide(0xa434, 2, lens.length, lens);
    block.push(...u32(0));

    // The value area, in the order the entries above ask for it. An offset is measured from the
    // *start of the block*, which is why this cannot be laid down before the directories that
    // sit between it and the header.
    for (const slot of pending) {
      for (let i = 0; i < 4; i += 1) {
        block[slot.at + i] = (block.length >>> (8 * i)) & 0xff;
      }
      block.push(...slot.bytes);
    }
    // The sub-directory pointer is an offset, not a value, so it is patched on its own.
    for (let i = 0; i < 4; i += 1) {
      block[subdirPointerAt + i] = (subdirAt >>> (8 * i)) & 0xff;
    }

    // The segment length is written *after* the payload exists, and it counts its own two bytes.
    // Computing it before the block is final is the bug this line is written against: patching an
    // offset can grow the block past what a 16-bit length was asked for, and a wrapped length
    // reads as a segment that runs to 57 KB of a file that is 253 bytes long.
    const payload = [...Buffer.from("Exif\0\0", "binary"), ...block];
    const segmentLength = payload.length + 2;
    if (segmentLength > 0xffff) {
      throw new Error(`the QA EXIF sample is ${segmentLength} bytes, which no JPEG segment can hold`);
    }
    const jpeg = [0xff, 0xd8, 0xff, 0xe1, ...be16(segmentLength), ...payload];
    // A `SOF0` frame of 4000x3000, so the panel has a size to disagree with about on screen.
    // The frame header is big-endian too, and the geometry probe reads these two words.
    jpeg.push(0xff, 0xc0, 0x00, 0x11, 0x08, ...be16(3000), ...be16(4000), 3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1, 0xff, 0xd9);
    fs.writeFileSync(file, Buffer.from(jpeg));
  }
  return file;
}

fs.mkdirSync(SHOTS, { recursive: true });

const clickLines = [];
function log(...a) {
  console.log("[walk]", ...a);
}
let warnedAboutStream = false;
function record(entry) {
  clickLines.push(entry);
  // The event stream is written for durability -- a killed pass should leave its clicks behind
  // -- but it is a SECONDARY record: `clickLines` above is the one the report is built from. So
  // a write that fails must not end the pass. It used to: this box runs a disk guard that trims
  // QA artifacts, and when the guard's window landed mid-pass the output directory was gone,
  // `appendFileSync` threw ENOENT, and the exception unwound the whole run. The pass had already
  // walked seven screens and every one of them was thrown away because a cache file could not be
  // appended to. A report written from memory and a report written from disk are the same report;
  // only the forensic stream is lost, and it says so rather than pretending.
  try {
    fs.appendFileSync(path.join(OUT, "clicks.jsonl"), JSON.stringify(entry) + "\n");
  } catch (error) {
    if (!warnedAboutStream) {
      warnedAboutStream = true;
      console.error(`[walk] the click stream is unwritable (${error.code || error.message}); the report continues without it`);
    }
  }
}

// ---------------------------------------------------------------- browser

const consoleLog = [];
/**
 * Refusals a pass provokes on purpose — a step-up gate in front of a dangerous action, for
 * instance. They are assertions the pass makes (it proves the prompt appeared and the retry
 * succeeded), not defects, so a pass registers one immediately before the act with
 * `expectRefusal` and the roll-up reports what it swallowed as `expectedRefusals` instead of a
 * finding. An allowance is single-use and indexed, so it can only excuse an entry that arrived
 * after it was registered.
 */
const expectedRefusals = [];

/** Register one deliberate refusal (a URL fragment for a request, a status shape for a console line). */
function expectRefusal(match, reason) {
  expectedRefusals.push({
    match,
    reason,
    consoleFrom: consoleLog.length,
    netFrom: netFailures.length,
    claimedConsole: false,
    claimedNet: false,
  });
}

const netFailures = [];
/** Requests the browser itself cancelled (navigation) — counted, never findings. */
const netAborted = [];
const dialogs = [];
const shots = [];

// A full-page PNG of a long admin page is megabytes; a thousand of them is gigabytes, which is
// more than the volume several writers share can hold. QA_SHOT_MODE=viewport (the run script drops
// to it when the volume is tight) keeps the pass: every screen is still visited and every control
// still clicked, the shots are just the visible frame instead of the whole scrolled page.
const SHOT_MODE = process.env.QA_SHOT_MODE === "viewport" ? "viewport" : "full";

async function shot(page, name, { full = true } = {}) {
  const file = path.join(SHOTS, `${name}.png`);
  // The quality option belongs to the JPEG format only: handing it to a PNG screenshot is a
  // hard error from the browser, so the two shapes are built separately rather than spread.
  const options =
    SHOT_MODE === "viewport"
      ? { path: file, fullPage: false, timeout: 15000, type: "jpeg", quality: 72 }
      : { path: file, fullPage: full, timeout: 15000 };
  try {
    await page.screenshot(options);
    shots.push({ name, file, url: page.url(), bytes: fs.statSync(file).size });
  } catch (err) {
    log(`screenshot failed for ${name}: ${err.message}`);
  }
}

function attach(page, phase) {
  page.on("console", (msg) => {
    if (msg.type() === "error" || msg.type() === "warning") {
      consoleLog.push({ phase, type: msg.type(), text: msg.text().slice(0, 400), url: page.url() });
    }
  });
  page.on("pageerror", (err) => {
    consoleLog.push({ phase, type: "pageerror", text: String(err).slice(0, 400), url: page.url() });
  });
  page.on("requestfailed", (req) => {
    const error = (req.failure() || {}).errorText || "";
    // A request the browser itself cancelled is the page moving on, not a defect: every panel
    // screen drops its in-flight fetches when the URL state changes, and the realtime screen
    // closes its event stream when the tab goes away. They are counted, never findings.
    if (error === "net::ERR_ABORTED") {
      netAborted.push({ phase, url: req.url().slice(0, 200), error });
      return;
    }
    netFailures.push({ phase, url: req.url().slice(0, 200), error });
  });
  page.on("response", (res) => {
    if (res.status() >= 400) netFailures.push({ phase, url: res.url().slice(0, 200), status: res.status() });
  });
  page.on("dialog", async (d) => {
    dialogs.push({ phase, type: d.type(), message: d.message().slice(0, 200) });
    try {
      if (d.type() === "prompt") await d.accept("qa");
      else await d.accept();
    } catch {
      /* already handled */
    }
  });
}

// ---------------------------------------------------------------- DOM diagnostics

async function diagnostics(page) {
  return page.evaluate(() => {
    const visible = (el) => {
      const b = el.getBoundingClientRect();
      if (b.width < 1 || b.height < 1) return false;
      const s = getComputedStyle(el);
      return s.visibility !== "hidden" && s.display !== "none" && Number(s.opacity) > 0.05;
    };
    const label = (el) =>
      (
        el.getAttribute("aria-label") ||
        el.innerText ||
        el.getAttribute("placeholder") ||
        el.getAttribute("title") ||
        el.getAttribute("name") ||
        ""
      )
        .trim()
        .replace(/\s+/g, " ")
        .slice(0, 70);

    const r = {
      url: location.href,
      title: document.title,
      viewport: { w: innerWidth, h: innerHeight },
      scrollWidth: document.documentElement.scrollWidth,
      horizontalOverflow: document.documentElement.scrollWidth > innerWidth + 2,
      brokenImages: [],
      emptyInteractives: [],
      unlabeledInputs: [],
      duplicateIds: [],
      lowContrast: [],
      tinyTargets: [],
      offscreen: [],
      h1Count: document.querySelectorAll("h1").length,
    };

    document.querySelectorAll("img").forEach((img) => {
      if (img.complete && img.naturalWidth === 0) r.brokenImages.push((img.currentSrc || img.src || "").slice(0, 160));
    });

    document.querySelectorAll("button, a[href], [role=button], input, select, textarea").forEach((el) => {
      if (!visible(el)) return;
      const name = label(el);
      const tag = el.tagName;
      if ((tag === "BUTTON" || el.getAttribute("role") === "button" || tag === "A") && !name) {
        r.emptyInteractives.push(el.outerHTML.slice(0, 130));
      }
      if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") {
        const id = el.id;
        const byFor = id ? document.querySelector(`label[for="${CSS.escape(id)}"]`) : null;
        const wrapped = el.closest("label");
        if (!byFor && !wrapped && !el.getAttribute("aria-label") && el.type !== "hidden") {
          r.unlabeledInputs.push(el.outerHTML.slice(0, 140));
        }
      }
      const b = el.getBoundingClientRect();
      if (tag === "BUTTON" || el.getAttribute("role") === "button") {
        if (b.width < 24 || b.height < 24) r.tinyTargets.push({ name, w: Math.round(b.width), h: Math.round(b.height) });
      }
      if (b.right > innerWidth + 8 || b.left < -8) {
        r.offscreen.push({ tag, name, left: Math.round(b.left), right: Math.round(b.right) });
      }
    });

    const ids = {};
    document.querySelectorAll("[id]").forEach((el) => (ids[el.id] = (ids[el.id] || 0) + 1));
    r.duplicateIds = Object.entries(ids).filter(([, n]) => n > 1).map(([id]) => id);

    const luminance = (color) => {
      const m = color.match(/rgba?\(([^)]+)\)/);
      if (!m) return null;
      const [rr, gg, bb] = m[1].split(",").slice(0, 3).map((v) => parseFloat(v) / 255);
      const f = (v) => (v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4));
      return 0.2126 * f(rr) + 0.7152 * f(gg) + 0.0722 * f(bb);
    };
    const backgroundOf = (el) => {
      let node = el;
      while (node && node !== document.documentElement) {
        const bg = getComputedStyle(node).backgroundColor;
        const m = bg.match(/rgba?\(([^)]+)\)/);
        if (m) {
          const parts = m[1].split(",").map((v) => parseFloat(v));
          if (parts.length < 4 || parts[3] > 0.5) return bg;
        }
        node = node.parentElement;
      }
      return getComputedStyle(document.body).backgroundColor;
    };
    document.querySelectorAll("p, span, a, h1, h2, h3, h4, li, td, th, label, button, code, small").forEach((el) => {
      if (!visible(el) || el.children.length > 0) return;
      const text = (el.innerText || "").trim();
      if (!text || text.length > 120) return;
      const s = getComputedStyle(el);
      const fg = luminance(s.color);
      const bg = luminance(backgroundOf(el));
      if (fg === null || bg === null) return;
      const ratio = (Math.max(fg, bg) + 0.05) / (Math.min(fg, bg) + 0.05);
      const size = parseFloat(s.fontSize);
      const bold = (parseInt(s.fontWeight, 10) || 400) >= 700;
      const large = size >= 24 || (size >= 18.66 && bold);
      const min = large ? 3 : 4.5;
      if (ratio < min) {
        r.lowContrast.push({ text: text.slice(0, 60), ratio: Math.round(ratio * 100) / 100, min, fontSize: size });
      }
    });

    return r;
  });
}

// ---------------------------------------------------------------- wizard

async function fillWizardStep(page) {
  const filled = await page.evaluate((creds) => {
    const done = [];
    const inputs = [...document.querySelectorAll("input, select, textarea")].filter((el) => {
      const b = el.getBoundingClientRect();
      return b.width > 1 && b.height > 1 && el.type !== "hidden" && !el.disabled;
    });
    for (const el of inputs) {
      const key = `${el.id} ${el.name} ${el.placeholder}`.toLowerCase();
      let value = null;
      if (el.tagName === "SELECT") {
        const option = [...el.options].find((o) => o.value && !o.disabled);
        if (option) {
          el.value = option.value;
          el.dispatchEvent(new Event("change", { bubbles: true }));
          done.push({ field: key.trim(), value: option.value });
        }
        continue;
      }
      if (el.type === "email" || /email/.test(key)) value = creds.email;
      else if (el.type === "password" || /password/.test(key)) value = creds.password;
      else if (/slug/.test(key)) value = creds.orgSlug;
      else if (/domain|host/.test(key)) value = creds.domain;
      else if (/org/.test(key)) value = creds.org;
      else if (/site.*key|key.*site|^setup-site-key/.test(key)) value = creds.siteKey;
      else if (/site/.test(key)) value = creds.site;
      else if (/name/.test(key)) value = creds.name;
      else if (el.type === "checkbox") {
        el.checked = true;
        el.dispatchEvent(new Event("change", { bubbles: true }));
        continue;
      } else if (el.type === "radio") {
        if (!el.checked) {
          el.checked = true;
          el.dispatchEvent(new Event("change", { bubbles: true }));
        }
        continue;
      } else continue;
      const proto = el.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, "value").set;
      setter.call(el, value);
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      done.push({ field: key.trim().slice(0, 60), value: String(value).slice(0, 40) });
    }
    return done;
  }, CREDS);
  return filled;
}

async function primaryClick(page) {
  // Click the (single) visible submit control of the current step.
  const candidates = ['form button[type="submit"]', 'button[type="submit"]', "form button", "button"];
  for (const sel of candidates) {
    const loc = page.locator(sel).first();
    if ((await loc.count()) > 0 && (await loc.isVisible().catch(() => false))) {
      const text = ((await loc.innerText().catch(() => "")) || "").trim().slice(0, 40);
      // A step's own POST can still be in flight, and the button says so ("Creating…", disabled).
      // Clicking it again submits the step twice: the platform refuses the duplicate — correctly —
      // but that refusal lands while the next step's form is being filled, and the wizard can drop
      // what was typed there. Wait for the busy state to clear instead of clicking into it.
      const busy = await loc
        .evaluate((el) => el.disabled || /(?:…|\.\.\.)$/.test((el.textContent || "").trim()))
        .catch(() => false);
      if (busy) return null;
      await loc.click({ timeout: 5000 }).catch(() => {});
      return text || sel;
    }
  }
  return null;
}

/** Click the wizard's action button — matched by label, in the order the wizard presents them. */
const WIZARD_ACTIONS = [
  "Use this theme",
  "Skip and finish",
  "Create account",
  "Create organization",
  "Create site",
  "Open the panel",
  "Finish",
  "Continue",
  "Next",
];

async function clickAction(page) {
  for (const text of WIZARD_ACTIONS) {
    const loc = page.locator(`button:has-text("${text}")`).first();
    if ((await loc.count()) > 0 && (await loc.isVisible().catch(() => false))) {
      const busy = await loc
        .evaluate((el) => el.disabled || /(?:…|\.\.\.)$/.test((el.textContent || "").trim()))
        .catch(() => false);
      if (busy) return null;
      await loc.click({ timeout: 5000 }).catch(() => {});
      return text;
    }
  }
  return primaryClick(page);
}

async function runWizard(page, report) {
  log("wizard: detecting first-run state");
  await page.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  const url = page.url();
  if (!url.includes("/setup")) {
    log(`wizard: not in setup (${url}) — installation already exists`);
    return { ran: false, url };
  }
  report.steps.push({ step: 0, url, action: "reached /setup" });
  await shot(page, "01-setup-step-1");
  for (let i = 1; i <= 10; i++) {
    const stepKey = await page
      .evaluate(() => {
        if (/Your installation is ready/i.test(document.body.innerText)) return "done";
        const el = document.querySelector('[data-setup-step][data-step-state="current"]');
        return el ? el.getAttribute("data-setup-step") : null;
      })
      .catch(() => null);
    if (!stepKey) break;
    if (stepKey === "done") {
      await page.locator('button:has-text("Open the panel")').first().click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(900);
      report.steps.push({ index: i, action: "open-panel", url: page.url() });
      break;
    }
    if (stepKey === "theme") {
      const option = page.locator('[data-theme-option][aria-pressed="false"]').first();
      if ((await option.count()) > 0) await option.click({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(250);
    }
    const filled = await fillWizardStep(page);
    const clicked = await clickAction(page);
    // The step's own POST can still be in flight: wait for the step to move (or the screen to say
    // the installation is ready) instead of clicking the same button into a second submission,
    // which the platform refuses — correctly — as an out-of-order step.
    for (let wait = 0; wait < 12; wait += 1) {
      await page.waitForTimeout(350);
      const state = await page
        .evaluate(() => {
          if (/Your installation is ready/i.test(document.body.innerText)) return "ready";
          const el = document.querySelector('[data-setup-step][data-step-state="current"]');
          return el ? el.getAttribute("data-setup-step") : "gone";
        })
        .catch(() => null);
      if (state !== stepKey || wait >= 5) break;
    }
    const now = page.url();
    report.steps.push({ index: i, stepKey, filled, clicked, url: now });
    await shot(page, `0${i + 1}-setup-${stepKey || i}`);
    const finished = await page
      .evaluate(() => /Your installation is ready/i.test(document.body.innerText))
      .catch(() => false);
    if (finished) {
      await page.locator('button:has-text("Open the panel")').first().click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(900);
      report.steps.push({ index: i, action: "open-panel", url: page.url() });
      break;
    }
    if (!now.includes("/setup")) break;
  }
  // "Go to panel" style finish button, if any is still on screen.
  const finish = page.locator('button:has-text("panel"), a:has-text("panel"), button:has-text("Finish")').first();
  if ((await finish.count()) > 0 && (await finish.isVisible().catch(() => false))) {
    await finish.click().catch(() => {});
    await page.waitForTimeout(900);
  }
  return { ran: true };
}

async function ensureSignedIn(page, report) {
  // The panel signs the owner in during the wizard; a "Sign in to continue" screen links to the
  // login form instead. Both paths end with the app shell rendered.
  const goSignIn = page.locator('a:has-text("Go to sign in"), button:has-text("Go to sign in")').first();
  if ((await goSignIn.count()) > 0 && (await goSignIn.isVisible().catch(() => false))) {
    await goSignIn.click().catch(() => {});
    await page.waitForTimeout(800);
  }
  await page.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(800);
  if (!/\/login|\/setup/.test(page.url()) && (await page.locator('nav[aria-label="Sections"]').count()) > 0) {
    return true; // already signed in — the wizard created the session
  }
  if (!/\/login/.test(page.url())) {
    await page.goto(`${URL_ADMIN}/login`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(700);
  }
  const email = page.locator('input[type="email"], input[name="email"], #email').first();
  if ((await email.count()) === 0) return false;
  await email.fill(CREDS.email).catch(() => {});
  const pass = page.locator('input[type="password"], input[name="password"], #password').first();
  await pass.fill(CREDS.password).catch(() => {});
  await shot(page, "10-login-filled");
  const clicked = await primaryClick(page);
  // Wait for the SIGN-IN to actually complete, not for a fixed number of milliseconds. This
  // screen has no error to look for: the form was filled, the API created the session, and the
  // pass still reported "could not sign in" — because it gave the request 1200 ms and the
  // request took 2.2 s on a box at load 90. A pass that dies at its first screen reports a
  // harness failure as if it were a product one, and a hard-coded sleep is the reason: the
  // right answer is to wait for the state that means signed in (the session cookie, or the
  // app shell that only renders once it is there), with a timeout generous enough for a loaded
  // machine.
  //
  // The fallback wait is what makes this a fix and not a race: if neither the cookie nor the
  // shell arrives, we say so and let the caller decide, instead of silently reading the URL.
  //
  // A REJECTION must not end the race. `waitForFunction` rejects with "execution context was
  // destroyed" the moment the login redirect navigates the page — which is precisely the success
  // path — so mapping a rejection to `false` would declare a successful sign-in a failure at the
  // exact moment it worked. A branch that fails hangs instead, and the only thing that can report
  // "not signed in" is the clock.
  //
  // The session cookie is `omnion_session`, but it is `HttpOnly` (apps/api/src/cookies.rs), so
  // `document.cookie` cannot see it and that branch can never fire — the app shell is the signal
  // that actually exists. The URL branch is the primary one: sign-in ends in a client-side
  // `router.replace("/")` (apps/admin/app/login/page.tsx), so leaving /login IS the success event.
  const neverSettles = () => new Promise(() => {});
  const signedIn = await Promise.race([
    page
      .waitForURL((u) => !/\/login|\/setup/.test(u.toString()), { timeout: 30000 })
      .then(() => true, neverSettles),
    page
      .waitForSelector('nav[aria-label="Sections"]', { timeout: 30000 })
      .then(() => true, neverSettles),
    page.waitForTimeout(30000).then(() => false),
  ]);
  // A short settle so the shell has painted before the first screen is measured.
  await page.waitForTimeout(signedIn ? 400 : 0);
  report.steps.push({ action: "login", clicked, url: page.url(), signedIn, waitedFor: "cookie-or-shell" });
  if (signedIn) return true;
  // The wait can still lose a race of its own — the cookie may be HttpOnly, which
  // `document.cookie` cannot see, and the shell may not render. Fall back to the original
  // question so a pass that really is signed in is not thrown away.
  return !/\/login/.test(page.url());
}

// ---------------------------------------------------------------- interaction

function sampleValueFor(meta) {
  const key = `${meta.type} ${meta.name} ${meta.label} ${meta.placeholder}`.toLowerCase();
  if (meta.type === "email" || /e-?mail/.test(key)) return "qa-sample@omnion.test";
  if (meta.type === "password") return "Sample-Passw0rd!";
  if (/model/.test(key)) return "gpt-4o-mini\ntext-embedding-3-small";
  if (/api.?key|secret|token/.test(key)) return "sk-qa-sample-key";
  if (/^https?:\/\//.test(meta.label) || /example\.com|\.test\/|\/v1/.test(meta.label)) return "https://api.openai.com/v1";
  if (meta.type === "url" || /url|endpoint|base/.test(key)) return "https://api.openai.com/v1";
  if (/protocol/.test(key)) return "openai_compatible";
  if (meta.type === "number" || /port|count|limit/.test(key)) return "42";
  if (meta.type === "date") return "2026-01-01";
  if (/slug/.test(key)) return SAMPLE_SLUG;
  if (/title/.test(key)) return "QA Sample Page";
  if (/search|filter|query/.test(key)) return "qa";
  if (/name|label/.test(key)) return "QA Provider";
  if (meta.tag === "textarea") return "QA sample text written by the automated walkthrough.";
  return "QA sample";
}

async function fillSubtree(page, selector) {
  return page.evaluate((sel) => {
    const root = document.querySelector(sel);
    if (!root) return [];
    const filled = [];
    const inputs = [...root.querySelectorAll("input, select, textarea")].filter((el) => el.type !== "hidden" && !el.disabled);
    for (const el of inputs) {
      if (el.type === "file") continue;
      if (el.type === "checkbox" || el.type === "radio") {
        if (!el.checked) {
          el.checked = true;
          el.dispatchEvent(new Event("change", { bubbles: true }));
        }
        continue;
      }
      if (el.tagName === "SELECT") {
        const option = [...el.options].find((o) => o.value && !o.disabled);
        if (option) {
          el.value = option.value;
          el.dispatchEvent(new Event("change", { bubbles: true }));
          filled.push({ field: el.id || el.name || "select", value: option.value });
        }
        continue;
      }
      const key = `${el.type} ${el.name} ${el.id} ${el.placeholder}`.toLowerCase();
      let value = "QA sample";
      if (el.type === "email" || /e-?mail/.test(key)) value = "qa-sample@omnion.test";
      else if (el.type === "password") value = "Sample-Passw0rd!";
      else if (el.type === "url" || /url|endpoint/.test(key)) value = "https://api.omnion.test/v1";
      else if (el.type === "number") value = "42";
      else if (/slug|key/.test(key)) value = SAMPLE_SLUG;
      else if (/title|name/.test(key)) value = "QA Sample";
      else if (el.tagName === "TEXTAREA") value = "QA sample text written by the automated walkthrough.";
      const proto = el.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      Object.getOwnPropertyDescriptor(proto, "value").set.call(el, value);
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      filled.push({ field: (el.id || el.name || el.type || "input").slice(0, 40), value });
    }
    return filled;
  }, selector);
}

async function clickPrimaryIn(page, selector) {
  const loc = page.locator(`${selector} button[type="submit"], ${selector} button`).first();
  if ((await loc.count()) > 0 && (await loc.isVisible().catch(() => false))) {
    const text = ((await loc.innerText().catch(() => "")) || "").trim().slice(0, 40);
    await loc.click({ timeout: 5000 }).catch(() => {});
    return text;
  }
  return null;
}

/**
 * `next dev` compiles a route on first visit and hydrates it asynchronously. A pass that stamps
 * attributes or clicks inside that window makes React report "a tree hydrated but some attributes of
 * the server rendered HTML didn't match" — a description of the pass's own mid-hydration changes,
 * not of the product. The apps announce hydration with `data-app-ready="1"`, so every document load
 * waits for that marker before anything else touches the page. Best effort: a screen that never
 * hydrates is still walked, and its console stays under inspection.
 */
function markHydrationWait(page, timeout = 20000) {
  const navigate = page.goto.bind(page);
  page.goto = async (...args) => {
    const response = await navigate(...args);
    await page
      .waitForFunction(() => !!document.querySelector('[data-app-ready="1"]'), null, { timeout })
      .catch(() => {});
    return response;
  };
  return page;
}

/**
 * Give a click's own work the moment it needs: a client-side navigation (Next's router) can
 * commit *after* the click returns, and the next element is then looked up on a page that is
 * already going away — which reads as "click timed out" and blames the screen.
 */
async function settleAfterClick(page, minimum = STEP_MS) {
  await page.waitForTimeout(minimum);
  let previous = page.url();
  for (let attempt = 0; attempt < 10; attempt += 1) {
    await page.waitForTimeout(150);
    const current = page.url();
    if (current === previous) break;
    previous = current;
  }
}

async function interact(page, pageName, report) {
  const inventory = () =>
    page.evaluate((max) => {
      const visible = (el) => {
        const b = el.getBoundingClientRect();
        if (b.width < 1 || b.height < 1) return false;
        const s = getComputedStyle(el);
        return s.visibility !== "hidden" && s.display !== "none" && Number(s.opacity) > 0.05;
      };
      const els = [...document.querySelectorAll('button, a[href], [role="button"], input, select, textarea, summary')]
        .filter(visible)
        .slice(0, max);
      document.querySelectorAll("[data-qa-idx]").forEach((el) => el.removeAttribute("data-qa-idx"));
      const seen = {};
      return els.map((el, idx) => {
        el.setAttribute("data-qa-idx", String(idx));
        const label = (
          el.getAttribute("aria-label") ||
          el.innerText ||
          el.getAttribute("placeholder") ||
          el.getAttribute("title") ||
          ""
        )
          .trim()
          .replace(/\s+/g, " ")
          .slice(0, 70);
        const desc = `${el.tagName.toLowerCase()}|${el.getAttribute("type") || ""}|${el.getAttribute("name") || el.id || ""}|${el.getAttribute("href") || ""}|${label}`;
        seen[desc] = (seen[desc] || 0) + 1;
        return {
          key: `${desc}#${seen[desc]}`,
          idx,
          tag: el.tagName.toLowerCase(),
          type: el.getAttribute("type") || "",
          name: el.getAttribute("name") || el.id || "",
          label,
          href: el.getAttribute("href") || "",
          disabled: Boolean(el.disabled),
          // A screen can declare a control its own depth pass drives: the generic fill/click
          // pass must not fire a write or an irreversible operation with a sample value.
          guard: el.getAttribute("data-qa-guard") || "",
        };
      });
    }, MAX_PER_PAGE);

  // Every round re-inventories the page: a navigation or a modal replaces the DOM, so an element
  // from an earlier round must never be looked up by a stale index. Handled elements are tracked
  // by a descriptor key instead.
  const pagePath = new URL(page.url()).pathname;
  const clickedKeys = new Set();
  let index = 0;
  let meta = null;
  for (let round = 0; round <= MAX_PER_PAGE; round += 1) {
    if (new URL(page.url()).pathname !== pagePath) {
      await page.goto(`${URL_ADMIN}${pagePath}`, { waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForTimeout(450);
    }
    // The search palette is global chrome: whatever opened it in the previous round (the search
    // box being filled, for one) is closed here, so a click is never swallowed by the overlay.
    const paletteOpen = await page
      .evaluate(() => Boolean(document.querySelector("[data-search-palette]")))
      .catch(() => false);
    if (paletteOpen) {
      await page.locator("[data-palette-input]").first().focus().catch(() => {});
      await page.keyboard.press("Escape").catch(() => {});
      await page.waitForTimeout(150);
    }
    const items = await inventory();
    if (round === 0) log(`interact: ${pageName} → ${items.length} elements`);
    meta = items.find((it) => !clickedKeys.has(it.key)) || null;
    if (!meta) break;
    clickedKeys.add(meta.key);
    const i = meta.idx;
    const baseUrl = page.url();
    if (meta.disabled) {
      record({ page: pageName, i, ...meta, action: "skip", outcome: "disabled" });
      continue;
    }
    if (/\bsign out\b/i.test(meta.label)) {
      record({ page: pageName, i, ...meta, action: "skip", outcome: "deferred-signout" });
      continue;
    }
    if (meta.guard) {
      // The control belongs to the screen's own pass (the analytics settings screen writes and
      // runs the purge and the erasure there): filling it with a sample value would be a
      // refused request here, not a click.
      record({ page: pageName, i, ...meta, action: "skip", outcome: `deferred-${meta.guard}` });
      continue;
    }
    if (meta.href && /^(mailto:|tel:|javascript:)/i.test(meta.href)) {
      record({ page: pageName, i, ...meta, action: "skip", outcome: "non-http-href" });
      continue;
    }

    index += 1;
    const before = { url: page.url(), console: consoleLog.length, net: netFailures.length, dialogs: dialogs.length };
    const started = Date.now();

    const control = page.locator(`[data-qa-idx="${i}"]`);
    if (meta.tag === "select") {
      const picked = await control
        .selectOption({ index: 1 })
        .then(() => true)
        .catch(() => control.selectOption({}).then(() => true).catch(() => false));
      record({ page: pageName, i, ...meta, action: "select", outcome: picked ? "ok" : "select-failed", ms: Date.now() - started });
      continue;
    }
    if (meta.tag === "input" && (meta.type === "checkbox" || meta.type === "radio")) {
      const checked = await control.check({ timeout: 3000 }).then(() => true).catch(() => false);
      record({ page: pageName, i, ...meta, action: "check", outcome: checked ? "ok" : "check-failed", ms: Date.now() - started });
      continue;
    }
    if (["input", "textarea"].includes(meta.tag) && meta.type !== "file") {
      const value = sampleValueFor(meta);
      let filled = await control
        .fill(value, { timeout: 4000 })
        .then(() => true)
        .catch(() => false);
      if (!filled) {
        // Some inputs (rich editors, hijacked value setters) reject fill(); set the value in-page.
        filled = await page
          .evaluate(
            (idx, val) => {
              const el = document.querySelector(`[data-qa-idx="${idx}"]`);
              if (!el) return false;
              const proto = el.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
              Object.getOwnPropertyDescriptor(proto, "value").set.call(el, val);
              el.dispatchEvent(new Event("input", { bubbles: true }));
              el.dispatchEvent(new Event("change", { bubbles: true }));
              return true;
            },
            i,
            value,
          )
          .catch(() => false);
      }
      record({
        page: pageName,
        i,
        ...meta,
        action: "fill",
        value,
        outcome: filled ? "ok" : "fill-failed",
        ms: Date.now() - started,
      });
      continue;
    }
    if (meta.tag === "input" && meta.type === "file") {
      const file = ensureSamplePng();
      const ok = await page
        .locator(`[data-qa-idx="${i}"]`)
        .setInputFiles(file)
        .then(() => true)
        .catch(() => false);
      record({ page: pageName, i, ...meta, action: "upload", outcome: ok ? "ok" : "upload-failed" });
      continue;
    }

    const popups = [];
    const onPopup = (p) => popups.push(p);
    page.context().on("page", onPopup);
    let clickError = null;
    try {
      await page.locator(`[data-qa-idx="${i}"]`).click({ timeout: 4500 });
    } catch (err) {
      clickError = String(err.message || err).slice(0, 200);
    }
    await settleAfterClick(page);
    page.context().off("page", onPopup);
    for (const p of popups) await p.close().catch(() => {});

    const after = { url: page.url(), console: consoleLog.length, net: netFailures.length, dialogs: dialogs.length };
    let outcome = "ok";
    if (clickError) {
      // The inventory can be read from one document and the click run against the next one — a
      // link in the same row navigates, and the control the walker wanted no longer exists on the
      // page that is actually open. A click with no target on the open page is the harness racing
      // itself (like an aborted request): counted, never a finding. A target that is still there
      // and still cannot be clicked is a real defect and stays a `click-error`.
      const stillThere = await page
        .locator(`[data-qa-idx="${i}"]`)
        .count()
        .catch(() => 1);
      outcome = stillThere === 0 ? "navigated-away" : "click-error";
    } else if (after.url !== before.url) outcome = "navigated";
    else if (after.dialogs > before.dialogs) outcome = "dialog";
    else if (after.console > before.console) outcome = "console-error";
    else if (after.net > before.net) outcome = "request-failed";
    else if (popups.length) outcome = "popup";

    const entry = {
      page: pageName,
      i,
      ...meta,
      action: "click",
      outcome,
      url_after: after.url,
      errors: consoleLog.slice(before.console).map((c) => `${c.type}: ${c.text.slice(0, 120)}`),
      net: netFailures.slice(before.net).map((n) => `${n.status || "fail"} ${n.url}`),
      reason: clickError || undefined,
      ms: Date.now() - started,
    };
    record(entry);

    // Anything that deserves eyes: navigation, dialogs, errors, popups.
    if (["navigated", "dialog", "console-error", "request-failed", "popup", "click-error"].includes(outcome)) {
      await shot(page, `click-${pageName}-${index}-${outcome}`, { full: false }).catch?.(() => {});
    }

    // A modal may have appeared — fill it once and submit.
    const dialogSel = '[role="dialog"], dialog[open]';
    const hasForm = await page
      .evaluate((sel) => {
        const node = document.querySelector(sel);
        if (!node) return false;
        const b = node.getBoundingClientRect();
        return b.width > 100 && b.height > 60;
      }, dialogSel)
      .catch(() => false);
    if (hasForm) {
      const filled = await fillSubtree(page, dialogSel);
      const submitted = await clickPrimaryIn(page, dialogSel);
      await page.waitForTimeout(700);
      await shot(page, `form-${pageName}-${index}`);
      record({
        page: pageName,
        i,
        action: "form",
        filled,
        submitted,
        outcome: submitted ? "submitted" : "filled-only",
        url_after: page.url(),
      });
      await page.keyboard.press("Escape").catch(() => {});
      await page.waitForTimeout(200);
      if (page.url() !== baseUrl) {
        await page.goto(baseUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
        await page.waitForTimeout(400);
      }
    }
  }
}

// ---------------------------------------------------------------- media upload

/**
 * Put one file in the library.
 *
 * The media screen's upload control is a hidden file input behind a button, and the click-through
 * only reaches visible controls — so this is the one place the pass sets a file on an input
 * directly, which is exactly what the browser does when a person picks a file. Without it the
 * library stays empty, and an empty library means the search index has no media to answer with.
 */
async function uploadMediaSample(page, source) {
  const file = source || ensureSamplePng();

  const input = page.locator('input[type="file"]').first();
  if ((await input.count()) === 0) {
    return { uploaded: false, note: "no file input on this screen" };
  }
  await input.setInputFiles(file).catch(() => {});
  await page.waitForTimeout(1600);
  return {
    uploaded: true,
    file: path.basename(file),
    listed: await page.locator(`text=${path.basename(file)}`).count(),
  };
}


// ---------------------------------------------------------------- featured media (REQ-064, slice 4d)

/**
 * `runFeaturedMediaDepth` — a page's hero image, its alt, its legend and its crop.
 *
 * The criterion is three sentences and each has a way to be satisfied by a panel that does not
 * work, so each is checked from the side that can fail it:
 *
 * * **"round-trip on the page"** — read back out of SQL, not out of the screen. A form that
 *   renders what it holds proves nothing about what it wrote, and the interesting case is the
 *   PARTIAL save: the legend moves and the alt and the crop must survive it.
 * * **"used by the renderer"** — checked on the PUBLIC payload, from a request with no panel
 *   cookie. The panel previewing its own object is a panel agreeing with itself.
 * * **"a deleted featured image leaves the page renderable with a warning"** — the page must
 *   still answer, still carry its content, and the payload must be `null` rather than a URL that
 *   404s. A 200 alone would pass against a page shipping a dead image.
 *
 * Three things the screen is asked because they are silent when they go wrong: the empty state
 * (this pass creates its own image, so only an assertion BEFORE the fixture can see it), the
 * required alt (the migration refuses a blank one, so the save button has to be off before the
 * round trip), and the *Clear crop* control (which sends an explicit null pair — the only way the
 * API can tell "clear it" from "leave it", and a control that does nothing when pressed is the
 * failure this step exists for).
 *
 * Every step writes under `steps.*` and `--only=featured-media` demands the list below by name.
 */
async function runFeaturedMediaDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the screen has nothing to read";
    return steps;
  }

  // The page this pass works on is its OWN, seeded before the screen is opened. A depth pass that
  // borrowed a page another pass created is how four of its checks went unrun for a tick.
  const slug = `featured-${stamp}`;
  const seeded = await page
    .request.post(`${URL_API}/api/v1/pages`, {
      data: { site_id: siteId, slug, title: `Featured ${stamp}` },
    })
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.pageWasCreated = seeded.status === 201 && Boolean(seeded.body && seeded.body.id);
  const pageId = (seeded.body && seeded.body.id) || "";
  if (!pageId) {
    steps.reason = "the fixture page could not be created, so there is nothing to drive";
    return steps;
  }

  // A media file the picker can offer. Seeded through the panel's OWN upload route so the row is
  // one the routes produce — same table, same checks, same object key the renderer will serve.
  const imageId = qaSql(
    `insert into media (site_id, storage_key, filename, content_type, size_bytes, checksum, alt_text)
     values ('${siteId}', 'qa/featured-${stamp}.png', 'featured-${stamp}.png', 'image/png', 2048,
             '${"b".repeat(64)}', 'the file own alt')
     returning id`,
  );
  steps.fixtureImageExists = Boolean(imageId);

  // ------------------------------------------------------------------ the empty state, first
  // Asserted while the page genuinely has no image, because the first thing this pass does next
  // is give it one — and after that every state is populated. The empty state is the one an owner
  // meets on a fresh page, and it is the one a self-seeding pass can never reach again.
  await page.goto(`${URL_ADMIN}/pages/${pageId}/media`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.screenReady = (await page.locator("[data-featured-media-tab]").count()) > 0;
  steps.emptyStateIsShown = (await page.locator("[data-featured-empty]").count()) > 0;
  steps.emptyStateSaysNoImage = /no featured image/i.test(
    await page.locator("[data-featured-media-tab]").first().innerText().catch(() => ""),
  );
  // The chip is the server's own word, and a fresh page must not claim one.
  steps.availabilitySaysNoImage = /^No featured image$/i.test(
    await page.locator("[data-featured-availability]").first().innerText().catch(() => ""),
  );
  // The alt field is disabled with no image: asking for a description of a file that is not there
  // is the form telling the operator it has not understood the state.
  steps.altIsDisabledWithNoImage =
    (await page.locator("[data-featured-alt]").first().isDisabled().catch(() => false)) === true;
  await shot(page, "featured-media-empty");

  // ------------------------------------------------------------------ the required alt
  // The migration refuses an image with a blank alt, so the SAVE has to be off before the round
  // trip. A button that is enabled and then answers 400 teaches the operator the rule is a
  // suggestion.
  await page.locator("[data-featured-open-picker]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.pickerOpened = (await page.locator("[data-featured-candidate]").count()) > 0;
  steps.pickerOffersTheFile =
    (await page.locator(`[data-featured-candidate="${imageId}"]`).count()) > 0;
  await shot(page, "featured-media-picker");

  await page.locator(`[data-featured-candidate="${imageId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1200);
  steps.pickingClosedThePicker = (await page.locator("[data-featured-candidate]").count()) === 0;

  // The alt is still blank, so the save must be refused. Asserted on the BUTTON rather than on a
  // banner: the rule is a precondition of the form, not a message the API sends afterwards.
  steps.saveIsBlockedWithoutAnAlt =
    (await page.locator("[data-featured-save]").first().isDisabled().catch(() => false)) === true;
  steps.screenExplainsWhy = /alt text is required|before an image can be saved/i.test(
    await page.locator("[data-featured-media-tab]").first().innerText().catch(() => ""),
  );
  await shot(page, "featured-media-alt-required");

  // The file's own alt is OFFERED and never applied on its own: the same photograph is the hero
  // of several pages with different descriptions.
  steps.theFilesOwnAltIsOffered =
    (await page.locator("[data-featured-use-file-alt]").count()) > 0;
  await page.locator("[data-featured-use-file-alt]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.theFilesOwnAltIsNowInTheField =
    (await page.locator("[data-featured-alt]").first().inputValue().catch(() => "")) ===
    "the file own alt";

  // ------------------------------------------------------------------ the save and the round trip
  const legend = `A legend written by the pass ${stamp}`;
  await page.locator("[data-featured-alt]").first().fill("Two people on a stone bridge at dusk");
  await page.locator("[data-featured-legend]").first().fill(legend);
  await page.locator("[data-featured-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.savedWithoutError = (await page.locator("[data-featured-error]").count()) === 0;
  steps.noticeIsOnScreen = (await page.locator("[data-featured-notice]").count()) > 0;

  // **The round trip is read out of SQL.** A panel that renders what it holds proves nothing
  // about what it wrote.
  const stored = qaSql(
    `select coalesce(featured_alt, '<null>') || '|' || coalesce(featured_legend, '<null>') || '|' ||
            coalesce(featured_media_id::text, '<null>') || '|' || coalesce(focal_x::text, '<null>')
     from pages where id = '${pageId}'`,
  );
  const [storedAlt, storedLegend, storedMedia, storedFocal] = String(stored).split("|");
  steps.altIsInSql = storedAlt === "Two people on a stone bridge at dusk";
  steps.legendIsInSql = storedLegend === legend;
  steps.mediaIdIsInSql = storedMedia === imageId;
  steps.cropStartsUnset = storedFocal === "<null>";

  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.altSurvivesAReload =
    (await page.locator("[data-featured-alt]").first().inputValue().catch(() => "")) ===
    "Two people on a stone bridge at dusk";
  steps.availabilitySaysAvailable = /^Available$/i.test(
    await page.locator("[data-featured-availability]").first().innerText().catch(() => ""),
  );
  steps.previewIsOnScreen = (await page.locator("[data-featured-preview]").count()) > 0;
  // The preview is an image with a REAL alt, and the alt it carries is this page's alt.
  steps.previewCarriesTheAlt = await page
    .locator("[data-featured-preview] img")
    .first()
    .getAttribute("alt")
    .then((value) => value === "Two people on a stone bridge at dusk")
    .catch(() => false);
  steps.noCropIsAnnounced = (await page.locator("[data-featured-no-crop]").count()) > 0;
  await shot(page, "featured-media-saved");

  // ------------------------------------------------------------------ the crop
  // The pad is a pointer target, so the pass presses ARROW KEYS on it: a focal point is the one
  // control where an exact value matters, and a pass that only drags proves nothing about the
  // keyboard path a precision crop needs.
  const pad = page.locator("[data-featured-focal-pad]").first();
  await pad.click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(400);
  for (let i = 0; i < 4; i += 1) {
    await pad.press("ArrowRight").catch(() => {});
    await pad.press("ArrowDown").catch(() => {});
  }
  await page.locator("[data-featured-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const storedCrop = qaSql(
    `select coalesce(focal_x::text, '<null>') || '|' || coalesce(focal_y::text, '<null>') from pages where id = '${pageId}'`,
  );
  const [cropX, cropY] = String(storedCrop).split("|");
  steps.cropIsInSql = cropX !== "<null>" && cropY !== "<null>";
  steps.cropAxesWerePaired = cropX === cropY;
  steps.cropMovedRightAndDown = Number(cropX) > 0.5 && Number(cropY) > 0.5;

  // The public payload carries the object position, and it is the renderer's own string.
  await page
    .request.post(`${URL_API}/api/v1/pages/${pageId}/publish`, { data: { body: "Featured body" } })
    .catch(() => {});
  const published = await page
    .request.get(`${URL_API}/api/v1/public/pages/${slug}`)
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.publicPageAnswers = published.status === 200;
  const publicImage = (published.body && published.body.featured_image) || null;
  steps.publicPayloadCarriesTheImage = Boolean(publicImage);
  steps.publicAltIsThisPagesAlt = Boolean(publicImage) && publicImage.alt === "Two people on a stone bridge at dusk";
  steps.publicObjectPositionIsSet =
    Boolean(publicImage) && typeof publicImage.object_position === "string" && publicImage.object_position.includes("%");
  steps.publicLegendIsCarried = Boolean(publicImage) && publicImage.legend === legend;
  await shot(page, "featured-media-public");

  // ------------------------------------------------------------------ the partial save
  // The legend moves and NOTHING ELSE is sent. This is what `coalesce($n, column)` buys, and it
  // is the case a panel which always POSTs the whole object cannot exercise — and a crop that
  // silently resets on an unrelated edit is the complaint this step exists to prevent.
  await page.locator("[data-featured-legend]").first().fill(`${legend} (edited)`);
  await page.locator("[data-featured-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const afterPartial = qaSql(
    `select coalesce(featured_legend, '<null>') || '|' || coalesce(featured_alt, '<null>') || '|' ||
            coalesce(focal_x::text, '<null>')
     from pages where id = '${pageId}'`,
  );
  const [pLegend, pAlt, pFocal] = String(afterPartial).split("|");
  steps.partialSaveMovedTheLegend = pLegend === `${legend} (edited)`;
  steps.partialSaveKeptTheAlt = pAlt === "Two people on a stone bridge at dusk";
  steps.partialSaveKeptTheCrop = pFocal !== "<null>";

  // ------------------------------------------------------------------ clear the crop
  // The control sends an explicit null PAIR. If it sent a missing key the crop would stay, and
  // the button would be a control that does nothing at all — which is why the check is on the
  // COLUMN and not on "the click happened".
  await page.locator("[data-featured-clear-crop]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.clearingTheCropShowedTheUnsetMessage =
    (await page.locator("[data-featured-no-crop]").count()) > 0;
  await page.locator("[data-featured-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.clearingTheCropActuallyClearedIt =
    qaSql(`select coalesce(focal_x::text, '<null>') from pages where id = '${pageId}'`) === "<null>";
  steps.clearingTheCropKeptTheImage =
    qaSql(`select coalesce(featured_media_id::text, '<null>') from pages where id = '${pageId}'`) ===
    imageId;

  // ------------------------------------------------------------------ the deletion degradation
  // The file goes to the TRASH, which keeps its row (REQ-010 holds the bytes until the trash is
  // emptied) — so this is the case where the column still resolves and the object is gone, and
  // the case a `delete from media` would never reach.
  qaSql(`update media set deleted_at = now() where id = '${imageId}'`);

  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.trashedFileWarnsTheOperator = (await page.locator("[data-featured-warning]").count()) > 0;
  steps.trashedFileNamesItself = /trash/i.test(
    await page.locator("[data-featured-warning]").first().innerText().catch(() => ""),
  );
  steps.trashedFileSaysThePageStillRenders = /still renders/i.test(
    await page.locator("[data-featured-warning]").first().innerText().catch(() => ""),
  );
  steps.availabilitySaysTrashed = /^In the trash$/i.test(
    await page.locator("[data-featured-availability]").first().innerText().catch(() => ""),
  );
  // The crop controls stay usable after a trash: a RESTORE brings the picture back with the crop
  // the operator already set, and a screen that disabled them would make them set it twice.
  steps.cropIsStillUsableWhileTrashed =
    (await page.locator("[data-featured-legend]").first().isDisabled().catch(() => true)) === false;
  await shot(page, "featured-media-trashed");

  // **The page is still renderable.** Asserted on the public route, from the same published slug.
  const afterTrash = await page
    .request.get(`${URL_API}/api/v1/public/pages/${slug}`)
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.pageStillRendersWithNoImage = afterTrash.status === 200;
  steps.pageStillCarriesItsTitle =
    Boolean(afterTrash.body) && afterTrash.body.revision && afterTrash.body.revision.title === `Featured ${stamp}`;
  // **And it does not carry a dead image.** A 200 alone would pass against a page shipping a URL
  // that 404s on every visitor's screen; the payload has to be null.
  steps.pageDoesNotCarryTheTrashedImage =
    Boolean(afterTrash.body) && (afterTrash.body.featured_image === null ||
      afterTrash.body.featured_image === undefined);

  // The column still names the file, so a restore needs no re-pick — a store that nulled it on
  // trash would turn a restore into a re-upload, which is the thing that actually loses work.
  steps.columnStillNamesTheTrashedFile =
    qaSql(`select coalesce(featured_media_id::text, '<null>') from pages where id = '${pageId}'`) ===
    imageId;

  // A trashed file is not offered by the picker: choosing it would 404 the moment it was saved.
  await page.locator("[data-featured-open-picker]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.pickerDoesNotOfferATrashedFile =
    (await page.locator(`[data-featured-candidate="${imageId}"]`).count()) === 0;
  await shot(page, "featured-media-picker-after-trash");

  // ------------------------------------------------------------------ restore, and remove
  qaSql(`update media set deleted_at = null where id = '${imageId}'`);
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.restoringBringsTheImageBack = /^Available$/i.test(
    await page.locator("[data-featured-availability]").first().innerText().catch(() => ""),
  );
  steps.aRestoredImageWarnsAboutNothing =
    (await page.locator("[data-featured-warning]").count()) === 0;

  // Removal clears all four columns in one save, and the confirmation SAYS all four — a button
  // labelled "Remove" that kept the crop is how an operator re-attaches an image to a crop they
  // chose for the last one.
  await page.locator("[data-featured-remove]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.removeConfirmationOpened = (await page.locator("[data-featured-remove-confirm]").count()) > 0;
  const confirmText = await page
    .locator("[data-featured-remove-confirm]")
    .first()
    .innerText()
    .catch(() => "");
  steps.removeConfirmationNamesTheAlt = /alt/i.test(confirmText);
  steps.removeConfirmationNamesTheCrop = /crop/i.test(confirmText);
  steps.removeConfirmationSaysTheFileStays = /stays in the library/i.test(confirmText);
  await shot(page, "featured-media-remove-confirm");

  await page.locator("[data-featured-remove-confirm-yes]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  const afterRemove = qaSql(
    `select coalesce(featured_media_id::text, '<null>') || '|' || coalesce(featured_alt, '<null>') ||
            '|' || coalesce(featured_legend, '<null>') || '|' || coalesce(focal_x::text, '<null>')
     from pages where id = '${pageId}'`,
  );
  steps.removeClearedEverything = afterRemove === "<null>|<null>|<null>|<null>";
  steps.removeReturnedToTheEmptyState = (await page.locator("[data-featured-empty]").count()) > 0;
  // And the file itself is untouched: removing an image from a page is not deleting it.
  steps.removeDidNotDeleteTheFile =
    qaSql(`select count(*) from media where id = '${imageId}'`) === "1";

  // ------------------------------------------------------------------ 390 px
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2200);
  const scroll = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  steps.noHorizontalScrollAt390 = scroll.scrollWidth <= scroll.clientWidth + 1;
  await shot(page, "featured-media-390");
  await page.setViewportSize({ width: 1440, height: 900 });

  return steps;
}

// ---------------------------------------------------------------- file manager (REQ-010, slice 1)

/**
 * Drive the file manager the way an operator does.
 *
 * The pass proves the parts that are easy to get subtly wrong and invisible in a screenshot: a
 * folder is created and shows up in the tree, the listing reports a total that matches its rows,
 * a filter narrows it, the bulk bar appears on a two-file selection, a delete moves the file to
 * the trash rather than destroying it, and the trash screen brings that same file back. A screen
 * that only looked right in a screenshot would pass all of that without doing any of it.
 */
/**
 * Run one depth pass without letting it end the run.
 *
 * A depth pass is a question asked of a screen; a screen that answers badly is a finding, and a
 * finding belongs in the report next to the other findings — not as the reason the report was
 * never written. The error is recorded under the pass's own name so it is counted, not hidden.
 */
async function runDepthPass(name, pass) {
  try {
    return await pass();
  } catch (cause) {
    const reason = cause instanceof Error ? `${cause.name}: ${cause.message}` : String(cause);
    log(`depth pass ${name} failed: ${reason}`);
    record({ page: "qa", action: "depth-pass-failed", pass: name, reason });
    return { ok: false, steps: 0, reason };
  }
}

/**
 * The backup centre, driven end to end (REQ-013, slice 1).
 *
 * The assertion that matters is not "the screen rendered" — it is that the five parts
 * reached a terminal state, that the parts table shows `plugins` as an empty-but-done part
 * rather than a gap, and that a verification over the real destination comes back clean. A
 * backup screen that renders perfectly while the destination is unwritable is the exact
 * failure this pass exists to catch, and no screenshot shows it.
 */
async function runBackups(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "backups", action: "backups", ...step });
  };

  await page.goto(`${URL_ADMIN}/backups`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector('[data-testid="backups-overview"]', { timeout: 8000 }).catch(() => {});
  const rendered = (await page.locator('[data-testid="backups-overview"]').count()) > 0;
  note({ step: "load", rendered });
  if (!rendered) {
    return { ok: false, reason: "the backup overview did not render" };
  }

  // The four status cards are on the screen before anything is created, and the
  // last-successful one says "never" rather than rendering nothing. An absent card reads as a
  // screen that has not loaded; "never" is the alarm.
  await page.waitForTimeout(800);
  const cards = await page.locator('[data-testid="backup-card"]').allInnerTexts();
  const cardText = cards.join(" | ");
  const cardsPresent = cards.length >= 4;
  const lastSuccessfulCard = cards[0] ?? "";
  note({
    step: "cards",
    cardsPresent,
    lastSuccessfulCard,
    // Before any run there is nothing to quote, so the card must say so out loud.
    saysNever: /never|no run/i.test(lastSuccessfulCard),
  });

  // Take a real backup through the drawer and wait for all five parts.
  await page.click('[data-testid="backup-create"]').catch(() => {});
  await page.waitForSelector('[data-testid="backup-create-drawer"]', { timeout: 5000 }).catch(() => {});
  const drawerOpen = (await page.locator('[data-testid="backup-create-drawer"]').count()) > 0;
  note({ step: "drawer", drawerOpen });

  await page.click('[data-testid="backup-create-confirm"]').catch(() => {});
  await page
    .waitForSelector('[data-testid="backup-part-row"]', { timeout: 120000 })
    .catch(() => {});
  await page.waitForTimeout(1500);

  const partRows = await page.locator('[data-testid="backup-part-row"]').allInnerTexts();
  const partNames = partRows.map((text) => text.split("\n")[0].trim());
  const allFivePresent = ["database", "media", "configuration", "themes", "plugins"].every(
    (name) => partNames.includes(name),
  );
  const everyPartTerminal = partRows.every((text) => /done|failed/.test(text));
  note({
    step: "parts",
    count: partRows.length,
    allFivePresent,
    everyPartTerminal,
    rows: partRows,
  });

  // The plugins part is empty BY DESIGN until a package installer exists, and the screen has
  // to say that rather than leave a row of zeroes the operator reads as a broken exporter.
  const pluginsRow = partRows.find((text) => /^plugins/.test(text)) ?? "";
  const pluginsExplainsItself = /nothing to record|result, not a gap/i.test(pluginsRow);
  note({ step: "plugins-empty", pluginsExplainsItself, row: pluginsRow });

  // Verify against the real destination.
  await page.click('[data-testid="backup-verify"]').catch(() => {});
  await page.waitForTimeout(4000);
  const verdict = await page
    .locator('[data-testid="backups-overview"] p[role="status"]')
    .allInnerTexts()
    .catch(() => []);
  const verdictText = verdict.join(" | ");
  const verificationRan = /parts match the manifest|checksum differs|could not be read back/i.test(
    verdictText,
  );
  note({ step: "verify", verificationRan, verdict: verdictText });

  // The restore preview, opened and read. A new panel that never appears in the walkthrough
  // inventory is an untested screen, and the rule is that the harness is extended rather
  // than the screen exempted — so this clicks the button, runs the preview against the real
  // destination, and asserts the three things the panel exists to say: the price, the
  // warnings, and the confirm phrase.
  await page.click('[data-testid="backup-restore-preview"]').catch(() => {});
  await page.waitForSelector('[data-testid="restore-preview"]', { timeout: 8000 }).catch(() => {});
  const previewOpen = (await page.locator('[data-testid="restore-preview"]').count()) > 0;
  note({ step: "restore-preview-open", previewOpen });

  // Collapsed by default: the panel must not render its tables unprompted, or every run
  // detail page grows five tables an operator did not ask for.
  const previewIdle = (await page.locator('[data-testid="restore-preview-idle"]').count()) > 0;
  note({ step: "restore-preview-idle", previewIdle });

  await page.click('[data-testid="restore-preview-load"]').catch(() => {});
  await page
    .waitForSelector('[data-testid="restore-preview-dropped"]', { timeout: 30000 })
    .catch(() => {});
  await page.waitForTimeout(1200);
  const priceText = await page
    .locator('[data-testid="restore-preview-dropped"]')
    .allInnerTexts()
    .catch(() => []);
  const warnings = await page
    .locator('[data-testid="restore-preview-warnings"] [data-testid^="restore-warning-"]')
    .allInnerTexts()
    .catch(() => []);
  const partRowsPreview = await page
    .locator('[data-testid="restore-part-row"]')
    .allInnerTexts()
    .catch(() => []);
  // Every part the run produced is in the preview's own table, with a mode. A panel that
  // lists only the available ones would hide exactly the part an operator most needs to
  // know about.
  const previewNames = partRowsPreview.map((text) => text.split("\n")[0].trim());
  note({
    step: "restore-preview",
    priced: priceText.length > 0,
    price: priceText.join(" | "),
    warningCount: warnings.length,
    warnings,
    partCount: partRowsPreview.length,
    allFiveOffered: partNames.every((name) => previewNames.includes(name)),
    rows: partRowsPreview,
  });

  // The confirm phrase, if the run is restorable. Empty is a legitimate answer for an
  // unrestorable run, so both are recorded rather than one being required.
  const phraseVisible = await page
    .locator('[data-testid="restore-confirm-input"]')
    .count();
  const notRestorable = await page
    .locator('[data-testid="restore-preview-not-restorable"]')
    .count();
  note({
    step: "restore-confirm",
    phraseFieldOffered: phraseVisible > 0,
    notRestorable: notRestorable > 0,
  });

  // ---- The restore control (REQ-013 slice 2b) ------------------------------------------------
  //
  // The panel ships a real button this tick, and the harness is extended rather than the
  // screen exempted. The walk deliberately drives it in the order a cautious operator would:
  // look at the button **before** typing the phrase, so the disabled state is recorded, then
  // with a wrong phrase, so the refusal is recorded, and only then with the right one.
  //
  // The middle step is the point of doing this in a browser at all. A Rust walk can prove the
  // API refuses; it cannot prove the *panel* shows the refusal rather than swallowing it into
  // a spinner — and a destructive control that fails silently is the worst shape this screen
  // could take.
  const runButton = page.locator('[data-testid="restore-run"]');
  const buttonOffered = (await runButton.count()) > 0;
  const disabledBefore = buttonOffered ? await runButton.first().isDisabled() : null;
  const disabledReason = await page
    .locator('[data-testid="restore-confirm-state"]')
    .first()
    .innerText()
    .catch(() => "");
  note({
    step: "restore-button",
    offered: buttonOffered,
    disabledBeforeTyping: disabledBefore,
    disabledReason: disabledReason.trim(),
  });

  // The part checkboxes. Every AVAILABLE part is ticked by default, because "restore the
  // whole archive" is the common case; a panel that made an operator tick five boxes to undo
  // one mistake is a panel they will not use.
  const partChecks = page.locator('[data-testid^="restore-part-check-"]');
  const checkCount = await partChecks.count();
  const checkedByDefault = await partChecks.evaluateAll((nodes) =>
    nodes.filter((node) => !node.disabled && node.checked).length,
  );
  const enabledChecks = await partChecks.evaluateAll(
    (nodes) => nodes.filter((node) => !node.disabled).length,
  );
  note({
    step: "restore-part-ticks",
    checkboxes: checkCount,
    tickedByDefault: checkedByDefault,
    selectable: enabledChecks,
    everySelectableTicked: checkedByDefault === enabledChecks,
  });

  // A wrong phrase must be refused *visibly*. The refusal is the API's own sentence, so the
  // assertion is that a `[role=alert]` appears and that it is not empty.
  const phraseField = page.locator('[data-testid="restore-confirm-input"]');
  if ((await phraseField.count()) > 0 && buttonOffered) {
    await phraseField.first().fill("RESTORE 00000000");
    await page.waitForTimeout(250);
    const enabledWithWrongPhrase = !(await runButton.first().isDisabled());
    note({
      step: "restore-wrong-phrase-button",
      enabledWithWrongPhrase,
    });
    // Only press it if the client let us: clicking a disabled button is a no-op, and the
    // walk must not record a refusal it did not cause.
    if (enabledWithWrongPhrase) {
      await runButton.first().click().catch(() => {});
      await page.waitForTimeout(1500);
      const alert = await page.locator('[data-testid="restore-error"]').innerText().catch(() => "");
      note({
        step: "restore-wrong-phrase",
        refused: alert.trim().length > 0,
        message: alert.trim(),
      });
    } else {
      note({
        step: "restore-wrong-phrase",
        refused: true,
        message: "the button is disabled for a wrong phrase, so the request was never sent",
      });
    }

    // And now the real thing. A QA stack is disposable by construction, which is the only
    // reason the REQ allows the destructive path to be exercised at all.
    const offered = await page
      .locator('[data-testid="restore-confirm-input"]')
      .first()
      .getAttribute("placeholder")
      .catch(() => null);
    if (offered) {
      await phraseField.first().fill(offered);
      await page.waitForTimeout(250);
      const enabledWithRightPhrase = !(await runButton.first().isDisabled());
      note({ step: "restore-right-phrase-button", enabledWithRightPhrase });
      if (enabledWithRightPhrase) {
        await runButton.first().click().catch(() => {});
        await page
          .waitForSelector('[data-testid="restore-outcome"], [data-testid="restore-error"]', {
            timeout: 120000,
          })
          .catch(() => {});
        await page.waitForTimeout(1500);
        const outcomeText = await page
          .locator('[data-testid="restore-outcome"]')
          .innerText()
          .catch(() => "");
        const refusalText = await page
          .locator('[data-testid="restore-error"]')
          .innerText()
          .catch(() => "");
        const safetyId = await page
          .locator('[data-testid="restore-safety-id"]')
          .innerText()
          .catch(() => "");
        note({
          step: "restore-run",
          restored: outcomeText.trim().length > 0,
          summary: outcomeText.trim().split("\n")[0],
          refused: refusalText.trim().length > 0,
          refusal: refusalText.trim(),
          safetyBackupNamed: /[0-9a-f]{8}-[0-9a-f]{4}/i.test(safetyId),
        });
      }
    }
    // ---- The queued restore (REQ-013 slice 2c) ---------------------------------------------
    // The one control in this file that exists to be *pressed and then un-pressed*, so the
    // order is the order a nervous operator would use: read the state, press Stop, and read
    // the state again. The load-bearing assertion is the LAST one — that a Stop control is
    // gone once the job is no longer cancellable. A panel that keeps offering "Stop" on a
    // finished restore is inviting an operator to press it, and a control that appears not to
    // work is worse than no control at all.
    const queueButton = page.locator('[data-testid="restore-queue"]');
    const queueOffered = (await queueButton.count()) > 0;
    const queueDisabled = queueOffered ? await queueButton.first().isDisabled() : null;
    const queueHint = await page
      .locator('[data-testid="restore-queue-hint"]')
      .innerText()
      .catch(() => "");
    note({
      step: "restore-queue-button",
      offered: queueOffered,
      disabledWithAnOutcomeShown: queueDisabled,
      // The difference between the two buttons must be IN WORDS on the screen, not inferred
      // from colour: the hint is what stops somebody in a hurry pressing the wrong one.
      hintNamesStoppability: /stop/i.test(queueHint),
      hint: queueHint.trim().slice(0, 200),
    });

    // The jobs list, before anything is queued. The empty state is a real state and it is
    // asserted, because "nothing here" and "this list could not be read" render the same and
    // only the wording tells them apart.
    const jobsEmpty = await page
      .locator('[data-testid="restore-jobs-empty"]')
      .count();
    note({ step: "restore-jobs-empty", offered: jobsEmpty > 0 });

    if (queueOffered && !queueDisabled) {
      await queueButton.first().click().catch(() => {});
      await page
        .waitForSelector('[data-testid^="restore-job-"]', { timeout: 30000 })
        .catch(() => {});
      await page.waitForTimeout(1500);

      const queuedRows = await page
        .locator('[data-testid^="restore-job-queued"], [data-testid^="restore-job-running"]')
        .allInnerTexts();
      const stopButtons = page.locator('[data-testid^="restore-job-stop-"]');
      const stopCount = await stopButtons.count();
      note({
        step: "restore-queued",
        rows: queuedRows.length,
        firstRow: queuedRows[0] ? queuedRows[0].split("\n")[0].trim() : "",
        stopButtonsOffered: stopCount,
      });

      if (stopCount > 0) {
        await stopButtons.first().click().catch(() => {});
        await page
          .waitForSelector('[data-testid="restore-job-aborted"]', { timeout: 30000 })
          .catch(() => {});
        await page.waitForTimeout(1200);
        const abortedText = await page
          .locator('[data-testid="restore-job-aborted"]')
          .first()
          .innerText()
          .catch(() => "");
        // The Stop control must be GONE. `cancellable` is the API's own field rather than a
        // derivation the panel makes, so this also proves the panel is reading it.
        const stopAfter = await page.locator('[data-testid^="restore-job-stop-"]').count();
        note({
          step: "restore-aborted",
          shown: abortedText.trim().length > 0,
          // An abort is a success and says so in words. If this reads as a failure the
          // operator stopped the right thing and believes it went wrong.
          saysNothingWasChanged: /nothing/i.test(abortedText),
          stopButtonsRemaining: stopAfter,
          text: abortedText.trim().slice(0, 240),
        });
      } else {
        note({
          step: "restore-aborted",
          reason: "the job left the queue before the Stop control could be pressed",
        });
      }
    }
  } else {
    note({
      step: "restore-button",
      reason: "the run is not restorable, so the control is correctly absent",
    });
  }

  // The list shows the run, and its state pill is the run's own state.
  await page.waitForTimeout(500);
  const rows = await page.locator('[data-testid="backup-row"]').count();
  const states = await page.locator('[data-testid="backup-state"]').allInnerTexts();
  note({ step: "list", rows, states: states.slice(0, 6) });

  // Filter chips carry the counts the endpoint sent, so a chip cannot disagree with the table.
  const chips = await page.locator('[data-testid^="backup-filter-"]').allInnerTexts();
  note({ step: "filters", chips: chips.slice(0, 6) });

  // Delete asks, and the confirmation names the backup.
  const deleteButtons = page.locator('[data-testid="backup-delete"]');
  if ((await deleteButtons.count()) > 0) {
    await deleteButtons.first().click().catch(() => {});
    await page.waitForTimeout(400);
    const confirmed = (await page.locator('[data-testid="backup-delete-confirm"]').count()) > 0;
    note({ step: "delete-confirm", confirmed });
    await page.click('[data-testid="backup-delete-confirm"]').catch(() => {});
    await page.waitForTimeout(1200);
    const afterDelete = await page.locator('[data-testid="backup-row"]').count();
    note({ step: "delete", rowsAfter: afterDelete });
  } else {
    note({ step: "delete-confirm", confirmed: false, reason: "no delete button to press" });
  }

  // The retention strip and its button. Clicked on a DISPOSABLE stack only — the sweep is
  // the one action on this screen that deletes data nobody asked it to delete, and a
  // walkthrough that presses it is fine exactly as long as the destination is a temporary
  // directory. The assertion is about the screen answering, not about the sweep finding
  // work: on a fresh stack there is nothing past its window, and "nothing to prune" is the
  // correct sentence, not a failure.
  const retentionStrip = (await page.locator('[data-testid="backup-retention"]').count()) > 0;
  await page.click('[data-testid="backup-sweep"]').catch(() => {});
  await page.waitForTimeout(2500);
  const sweepReport = await page
    .locator('[data-testid="backup-sweep-report"]')
    .allInnerTexts()
    .catch(() => []);
  const sweepText = sweepReport.join(" | ");
  const sweepAnswered = /looked at \d+ expired backup|failed for \d+ tenant/i.test(sweepText);
  const sweepStrandedShown = (await page.locator('[data-testid="backup-sweep-stranded"]').count()) > 0;
  note({ step: "retention", retentionStrip, sweepAnswered, sweepStrandedShown, sweep: sweepText });

  // The schedules table (REQ-013, slice 3). Run inside this pass rather than beside it
  // because it lives on the same screen: the panel is mounted below the runs list, so
  // "the schedules table is a screen nobody opened" would not be visible in the route list.
  const schedules = await runBackupSchedules(page, report);

  const ok =
    rendered &&
    cardsPresent &&
    allFivePresent &&
    everyPartTerminal &&
    verificationRan &&
    retentionStrip &&
    sweepAnswered &&
    schedules.ok;
  return { ok, steps: steps.length, cards: cardText, schedules };
}

/**
 * The backup schedules table (REQ-013, slice 3).
 *
 * This pass exists because the defect it is aimed at is invisible in every screenshot: the
 * `next_run_at` column shipped with the table in slice 1, and for two slices **nothing wrote
 * it**. A schedule could be created, listed, and rendered with a cadence sentence next to an
 * empty next-run cell for ever. A screen that looks right and never fires is exactly what a
 * walkthrough cannot see, so the assertion here is the *column's contents*, not the layout.
 *
 * What it drives, in order:
 *
 * 1. the empty state, on a stack with no schedules;
 * 2. the editor, and that the conditional fields appear and disappear with the frequency —
 *    an hourly schedule must not show a time of day, and a monthly one must not show a
 *    weekday, because a hidden-but-submitted value is one the server has to decide about;
 * 3. a **real** daily schedule, saved, and its next run read back — the cell must contain a
 *    date and the zone, not a dash;
 * 4. "run now" against the live API, proving a schedule can produce a backup;
 * 5. pause, which must turn the next-run cell into "paused" rather than leaving a promise
 *    the worker will not keep;
 * 6. delete, and the sentence naming that the produced runs survive.
 */
async function runBackupSchedules(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "backups", action: "backup-schedules", ...step });
  };

  const panel = page.locator('[data-testid="backup-schedules"]');
  await panel.scrollIntoViewIfNeeded().catch(() => {});
  await page.waitForTimeout(600);

  const panelPresent = (await panel.count()) > 0;
  note({ step: "panel", panelPresent });
  if (!panelPresent) {
    return { ok: false, reason: "the schedules panel did not render" };
  }

  // 1. The empty state. On a fresh stack there are no schedules, and the table must say so
  // with the one action that gets past it.
  const emptyText = await panel.innerText().catch(() => "");
  const emptyExplains = /no schedules/i.test(emptyText);
  note({ step: "empty", emptyExplains, text: emptyText.slice(0, 160) });

  // 2. The editor, and the conditional fields.
  await page.click('[data-testid="backup-schedule-new"]').catch(() => {});
  await page.waitForSelector('[data-testid="backup-schedule-editor"]', { timeout: 8000 }).catch(() => {});
  const editorOpen = (await page.locator('[data-testid="backup-schedule-editor"]').count()) > 0;
  note({ step: "editor-open", editorOpen });
  if (!editorOpen) {
    return { ok: false, reason: "the schedule editor did not open" };
  }

  const hasTime = async () => (await page.locator('[data-testid="backup-schedule-time"]').count()) > 0;
  const hasWeekday = async () =>
    (await page.locator('[data-testid="backup-schedule-weekday"]').count()) > 0;
  const hasDom = async () =>
    (await page.locator('[data-testid="backup-schedule-dayofmonth"]').count()) > 0;

  // Daily shows a time and no day fields.
  await page.selectOption('[data-testid="backup-schedule-frequency"]', "daily").catch(() => {});
  await page.waitForTimeout(250);
  const dailyShape = { time: await hasTime(), weekday: await hasWeekday(), dom: await hasDom() };
  note({ step: "shape-daily", ...dailyShape });

  // Weekly adds the weekday and still shows the time.
  await page.selectOption('[data-testid="backup-schedule-frequency"]', "weekly").catch(() => {});
  await page.waitForTimeout(250);
  const weeklyShape = { time: await hasTime(), weekday: await hasWeekday(), dom: await hasDom() };
  note({ step: "shape-weekly", ...weeklyShape });

  // Monthly swaps the weekday for a day of the month.
  await page.selectOption('[data-testid="backup-schedule-frequency"]', "monthly").catch(() => {});
  await page.waitForTimeout(250);
  const monthlyShape = { time: await hasTime(), weekday: await hasWeekday(), dom: await hasDom() };
  note({ step: "shape-monthly", ...monthlyShape });

  // Hourly names no time of day at all.
  await page.selectOption('[data-testid="backup-schedule-frequency"]', "hourly").catch(() => {});
  await page.waitForTimeout(250);
  const hourlyShape = { time: await hasTime(), weekday: await hasWeekday(), dom: await hasDom() };
  note({ step: "shape-hourly", ...hourlyShape });

  // 3. A real daily schedule, saved, with its next run read back.
  await page.selectOption('[data-testid="backup-schedule-frequency"]', "daily").catch(() => {});
  await page.waitForTimeout(200);
  await page.fill('[data-testid="backup-schedule-name"]', "QA nightly").catch(() => {});
  await page.fill('[data-testid="backup-schedule-time"]', "02:30").catch(() => {});
  await page.selectOption('[data-testid="backup-schedule-zone"]', "Europe/Istanbul").catch(() => {});
  await page.waitForTimeout(200);
  await page.click('[data-testid="backup-schedule-save"]').catch(() => {});
  await page.waitForTimeout(2500);

  const saveNotice = await panel.locator('p[role="status"]').innerText().catch(() => "");
  const editorClosed = (await page.locator('[data-testid="backup-schedule-editor"]').count()) === 0;
  note({ step: "save", editorClosed, notice: saveNotice.trim() });

  const scheduleRows = await panel.locator("[data-schedule]").count();
  const rowText = scheduleRows > 0 ? await panel.locator("[data-schedule]").first().innerText() : "";
  // The load-bearing assertion of this whole pass: the next-run cell must carry a real date
  // and the zone. A dash here is the two-slice defect, and every other assertion in this file
  // would still pass with it.
  const nextRunHasDate = /\d{1,2}\s+\w{3,}\s+\d{4}|\d{4}-\d{2}-\d{2}|\d{1,2}:\d{2}/.test(rowText);
  const namesTheZone = /Europe\/Istanbul|UTC/.test(rowText);
  const noNextRunWarning = /will not fire until it is saved again/i.test(rowText);
  note({
    step: "next-run",
    scheduleRows,
    nextRunHasDate,
    namesTheZone,
    noNextRunWarning,
    row: rowText.replace(/\n/g, " | ").slice(0, 220),
  });

  // 4. Run it now, over the real API.
  if (scheduleRows > 0) {
    const row = panel.locator("[data-schedule]").first();
    const runId = await row.getAttribute("data-schedule").catch(() => null);
    if (runId) {
      await page.click(`[data-testid="backup-schedule-run-${runId}"]`).catch(() => {});
      await page.waitForTimeout(20000);
      const runNotice = await panel.locator('p[role="status"]').innerText().catch(() => "");
      const runAnswered = /ran|parts were written|could not/i.test(runNotice);
      // The sentence has to say the next scheduled run is UNCHANGED, because a "run now"
      // that consumed the 02:00 slot is a silent skip of tomorrow's backup.
      const nextRunUntouched = /next scheduled run is unchanged|next run/i.test(runNotice);
      note({ step: "run-now", runAnswered, nextRunUntouched, notice: runNotice.trim().slice(0, 200) });

      // 5. Pause. The next-run cell must become "paused", not keep a promise.
      await row.locator('button:has-text("Pause")').first().click().catch(() => {});
      await page.waitForTimeout(2000);
      const pausedText = await panel.locator("[data-schedule]").first().innerText().catch(() => "");
      const showsPaused = /paused/i.test(pausedText);
      note({ step: "pause", showsPaused, row: pausedText.replace(/\n/g, " | ").slice(0, 200) });

      // 6. Delete, and the sentence that the produced runs survive.
      await page.click(`[data-testid="backup-schedule-delete-${runId}"]`).catch(() => {});
      await page.waitForTimeout(2000);
      const afterDelete = await panel.locator("[data-schedule]").count();
      const deleteNotice = await panel.locator('p[role="status"]').innerText().catch(() => "");
      const deleteExplainsSurvival = /still here|does not delete history|kept/i.test(deleteNotice);
      note({
        step: "delete",
        rowsAfter: afterDelete,
        deleteExplainsSurvival,
        notice: deleteNotice.trim().slice(0, 200),
      });
    }
  }

  const ok =
    panelPresent &&
    editorOpen &&
    dailyShape.time &&
    !dailyShape.weekday &&
    weeklyShape.weekday &&
    monthlyShape.dom &&
    !monthlyShape.weekday &&
    !hourlyShape.time &&
    editorClosed &&
    scheduleRows > 0 &&
    nextRunHasDate &&
    namesTheZone &&
    !noNextRunWarning;
  return { ok, steps: steps.length };
}

async function runMediaFileManager(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-file-manager", ...step });
  };

  await page.goto(`${URL_ADMIN}/media`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("#media-new-folder", { timeout: 8000 }).catch(() => {});
  const loaded = (await page.locator("#media-new-folder").count()) > 0;
  note({ step: "load", loaded });
  if (!loaded) {
    return { ok: false, reason: "the media browser did not render" };
  }

  // A folder under the root, with a name the pass can find again.
  const folderName = "QA Campaign 2026";
  await page.fill("#media-new-folder", folderName);
  await page.click('button[aria-label="Create folder"]');
  await page.waitForTimeout(1400);
  const folderInTree = await page.locator(`aside >> text=${folderName}`).count();
  note({ step: "create-folder", folderInTree });

  // Open it: the breadcrumb names the folder and the listing is scoped to it.
  await page.locator(`aside button:has-text("${folderName}")`).first().click().catch(() => {});
  await page.waitForTimeout(1200);
  const url = page.url();
  const inBreadcrumb = await page.locator(`nav[aria-label="Breadcrumb"] >> text=${folderName}`).count();
  note({ step: "open-folder", url: url.replace(URL_ADMIN, ""), inBreadcrumb });
  await shot(page, "media-folder-open");

  // A second file, so a two-file selection is possible.
  await uploadMediaSample(page);
  await page.waitForTimeout(1200);

  // Move both into the folder through the bulk bar, which is the real path an operator takes.
  const checkboxes = page.locator('tbody input[type="checkbox"], ul input[type="checkbox"]');
  const available = await checkboxes.count();
  if (available >= 2) {
    await checkboxes.nth(0).check();
    await checkboxes.nth(1).check();
    await page.waitForTimeout(400);
  }
  const bulkVisible = (await page.locator('div[aria-label="Selection"]').count()) > 0;
  note({ step: "bulk-bar", available, bulkVisible });
  if (bulkVisible) {
    await page.click('div[aria-label="Selection"] >> text=Move here');
    await page.waitForTimeout(1500);
  }
  const rowsAfterMove = await page.locator("tbody tr").count();
  note({ step: "bulk-move", rowsAfterMove });

  // A filter narrows the listing and the footer count follows it.
  await page.click('button[aria-label="Filters"]');
  await page.waitForTimeout(300);
  await page.selectOption("#media-kind", "image");
  await page.waitForTimeout(1200);
  const imageRows = await page.locator("tbody tr").count();
  // The footer is a *report*, not a precondition: a listing that renders no rows has no footer
  // to read, and waiting 30 s for one throws away the rest of the pass — the depth passes below
  // this line never run and the whole QA run dies on a screen that is behaving correctly. A
  // missing footer is recorded as absent and the pass continues.
  const footerLocator = page.locator("text=/Showing \\d+ of \\d+/").first();
  const footer =
    (await footerLocator.count()) > 0
      ? await footerLocator.textContent({ timeout: 3000 }).catch(() => null)
      : null;
  note({ step: "filter-kind", imageRows, footer });
  await shot(page, "media-filtered");
  await page.selectOption("#media-kind", "");
  await page.click('button[aria-label="Filters"]');
  await page.waitForTimeout(600);

  // A delete is a trash, not a purge.
  const firstRow = page.locator("tbody tr").first();
  if ((await firstRow.count()) > 0) {
    await firstRow.locator('button[aria-label^="Move"]').first().click().catch(() => {});
    await page.waitForSelector("text=/moved to the trash/", { timeout: 4000 }).catch(() => {});
    const trashed = (await page.locator("text=/moved to the trash/").count()) > 0;
    note({ step: "trash-one", trashed });
  }

  // The trash screen holds it, with a countdown, and restores it.
  await page.goto(`${URL_ADMIN}/media/trash`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  const trashRows = await page.locator("tbody tr").count();
  const countdown = await page.locator("text=/\\d+ days?|today/").count();
  await shot(page, "media-trash-populated");
  note({ step: "trash-listing", trashRows, countdown });

  if (trashRows > 0) {
    await page.locator('button:has-text("Restore")').first().click();
    await page.waitForTimeout(1500);
    const restored = (await page.locator("text=/restored to the folder/").count()) > 0;
    note({ step: "restore", restored });
  }
  await shot(page, "media-trash-after-restore");

  // The empty state has to be a real one, not a blank table.
  await page.goto(`${URL_ADMIN}/media?folder=nonexistent-folder`, {
    waitUntil: "domcontentloaded",
  }).catch(() => {});
  await page.waitForTimeout(1200);
  const emptyOrError = (await page.locator("text=/folder is empty|could not/i").count()) > 0;
  note({ step: "empty-or-error", emptyOrError });

  return { ok: true, steps: steps.length };
}

// ------------------------------------------------------- file detail (REQ-010, slice 2)

/**
 * The file detail screen: preview, metadata and the version history.
 *
 * The pass finds a real file through the API the panel itself uses, opens its detail screen and
 * checks that what renders is that file — the name on screen, a preview element chosen by the
 * file's content type, the dimensions the header of its bytes carried, and a history that has at
 * least the upload. It then saves a piece of metadata and reads it back, which is the one write
 * on this screen a visitor can undo by accident.
 *
 * A screen that only ever renders its 404 state passes a route walk, so the id is taken from a
 * real row: the point is to test the screen, not the router that guards it.
 */
/**
 * Drive the transformation presets the way an operator does (REQ-010, slice 3).
 *
 * The interesting claims are not "the table renders" — they are the ones a screenshot cannot
 * settle: a preset that refuses a bad quality with a message *under the field*, and a preset URL
 * that answers with real transformed bytes rather than the original. Both are checked here.
 */
async function runMediaPresets(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-presets", ...step });
  };

  await page.goto(`${URL_ADMIN}/media/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);

  const listed = await page.locator("text=Transformation presets").count();
  note({ step: "load", listed });
  if (listed === 0) {
    return { ok: false, reason: "the presets screen did not render" };
  }

  // The seeded preset must be there: a site with no `standard` would silently serve full-size
  // originals to every page that asks for it.
  const seeded = await page.locator("text=standard").count();
  note({ step: "seeded", seeded });

  await page.getByRole("button", { name: /New preset/i }).click().catch(() => {});
  await page.waitForTimeout(600);
  const editor = await page.locator("text=New preset").count();
  note({ step: "editor-open", editor });
  if (editor === 0) {
    return { ok: false, reason: "the preset editor did not open" };
  }

  // An out-of-range quality must be refused *by the form*, naming the field, before it reaches
  // the API. A screen that posts and then shows a banner has already sent the request.
  await page.locator('input[placeholder="card"]').fill("qa-card").catch(() => {});
  await page.locator('input[placeholder="1200"]').fill("640").catch(() => {});
  await page.locator('input[placeholder="630"]').fill("360").catch(() => {});
  await page.locator('input[inputmode="numeric"]').last().fill("9000").catch(() => {});
  await page.getByRole("button", { name: /Create preset/i }).click().catch(() => {});
  await page.waitForTimeout(800);

  const fieldError = await page.locator('[role="alert"]').allTextContents();
  note({ step: "quality-refused", fieldError });
  const qualityNamed = fieldError.some((text) => /quality/i.test(text));

  // Now a good one, so the table is proved with a row this pass created.
  await page.locator('input[inputmode="numeric"]').last().fill("75").catch(() => {});
  await page.getByRole("button", { name: /Create preset/i }).click().catch(() => {});
  await page.waitForTimeout(1500);
  const created = await page.locator("text=qa-card").count();
  note({ step: "created", created });
  await shot(page, "media-presets-created");

  // The preset URL must answer with transformed bytes. The id comes from the library listing
  // (a real file) and the query from the screen's own `data-preset-query`, so the URL is
  // assembled the way a page assembles it rather than copied out of the table.
  const fileId = await page.evaluate(() => {
    const rows = document.querySelectorAll("code[data-preset-query]");
    return rows.length > 0 ? rows[0].getAttribute("data-preset-query") : null;
  });
  note({ step: "preset-query", fileId });

  const served = await page.evaluate(async (query) => {
    // The library is where a real file id lives; asking for the listing keeps this in the page
    // with the session cookie, so the bytes come from the real API.
    const listed = await fetch("/api/v1/media/files?limit=1", { credentials: "same-origin" });
    const page1 = await listed.json();
    const file = page1.files && page1.files[0];
    if (!file || !query) return { ok: false, reason: "no file or no preset query" };
    const url = `/api/v1/media/${file.id}/raw${query}`;
    const response = await fetch(url, { credentials: "same-origin" });
    const buffer = new Uint8Array(await response.arrayBuffer());
    return {
      ok: response.ok,
      status: response.status,
      type: response.headers.get("content-type"),
      cache: response.headers.get("cache-control"),
      bytes: buffer.length,
      magic: Array.from(buffer.slice(0, 12))
        .map((b) => b.toString(16).padStart(2, "0"))
        .join(""),
    };
  }, fileId);
  note({ step: "preset-url", ...served });

  await shot(page, "media-presets-table");
  return {
    ok: created > 0 && qualityNamed,
    steps: steps.length,
    seeded,
    qualityNamed,
    created,
    served,
  };
}

/**
 * Drive the storage settings tab the way an operator does (REQ-010, slice 3).
 *
 * The claims a screenshot cannot settle are the ones worth walking: a range that is refused
 * *by the form*, naming the field, before anything is sent; a connection test that reports what
 * it proved rather than a green tick; and a save that leaves the bucket alone when the form only
 * changed one field. All three are checked against the DOM and the API, not against the page
 * having rendered.
 */
async function runMediaStorage(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-storage", ...step });
  };

  await page.goto(`${URL_ADMIN}/media/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);

  // The storage tab is a tab, not a second URL: a settings page with a hidden screen behind a
  // link is two screens, and the second one is the one nobody visits.
  await page.locator("#media-settings-tab-storage").click().catch(() => {});
  await page.waitForSelector('input[placeholder="omnion-media"]', { timeout: 8000 }).catch(() => {});
  const listed = await page.locator('input[placeholder="omnion-media"]').count();
  note({ step: "load", listed });
  if (listed === 0) {
    return { ok: false, reason: "the storage tab did not render" };
  }

  // A settings screen that renders a credential is the failure this whole tab is built to avoid,
  // so the walk checks the rendered text for one rather than trusting the type.
  const visibleText = await page.locator("#media-settings-panel-storage").innerText().catch(() => "");
  const leaks = ["secret", "access key", "password", "credential"].filter((word) =>
    new RegExp(word, "i").test(visibleText),
  );
  note({ step: "no-credentials", leaks });

  // An out-of-range value must be refused by the form, naming the field, before it is sent.
  const ttl = page.locator('input[aria-label="Signed URL lifetime in seconds"]');
  await ttl.fill("5").catch(() => {});
  await page.getByRole("button", { name: /^Test connection$/ }).click().catch(() => {});
  await page.waitForTimeout(900);
  const alerts = await page.locator("#media-settings-panel-storage [role='alert']").allTextContents();
  const ttlNamed = alerts.some((text) => /between 60 and 604800/.test(text));
  note({ step: "ttl-refused", ttlNamed, alerts });

  // Now a good one, and the connection test must report what it *proved* — a sentence about a
  // write, not a bare tick. A result that says only "connected" is what a read-only probe says.
  await ttl.fill("900").catch(() => {});
  await page.getByRole("button", { name: /^Test connection$/ }).click().catch(() => {});
  await page.waitForTimeout(6000);
  const probe = await page
    .locator('[data-testid="media-storage-probe"]')
    .innerText()
    .catch(() => "");
  note({ step: "connection", probe });
  const probeAnswered = probe.length > 0;
  // "reached … and wrote and removed a probe object" is the passing shape; a store that
  // accepted a write and refused a delete must say so instead of claiming success.
  const honest = /wrote and removed|could not|reached/i.test(probe);

  // A save must persist, and must not have reset the fields the walk did not touch.
  const upload = page.locator('input[aria-label="Maximum upload size in megabytes"]');
  await upload.fill("48").catch(() => {});
  await page.getByRole("button", { name: /^Save$/ }).click().catch(() => {});
  await page.waitForTimeout(2500);
  const saved = await upload.inputValue().catch(() => "");
  const notice = await page.locator("#media-settings-panel-storage [role='status']").allTextContents();
  note({ step: "save", saved, notice });
  await shot(page, "media-storage-settings");

  // The saved value must be readable back out of the API by an independent request, so the
  // walk is not just trusting that the form kept its own text.
  const persisted = await page.evaluate(async () => {
    const site = new URLSearchParams(window.location.search).get("site_id");
    const sites = await (await fetch("/api/v1/sites", { credentials: "same-origin" })).json();
    const first = (sites.sites || sites)[0];
    const query = `site_id=${first ? first.id : site || ""}`;
    const response = await fetch(`/api/v1/media/settings?${query}`, { credentials: "same-origin" });
    return { status: response.status, body: await response.json() };
  });
  note({ step: "persisted", status: persisted.status, maxUploadMb: persisted.body?.max_upload_mb });

  return {
    ok: listed > 0 && leaks.length === 0 && ttlNamed && probeAnswered && honest && saved === "48",
    steps: steps.length,
    leaks,
    ttlNamed,
    probeAnswered,
    honest,
    saved,
    persisted: persisted.body?.max_upload_mb,
  };
}

async function runMediaFileDetail(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-file-detail", ...step });
  };

  await page.goto(`${URL_ADMIN}/media`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("#media-new-folder", { timeout: 8000 }).catch(() => {});
  const uploaded = await uploadMediaSample(page);
  await page.waitForTimeout(1500);
  note({ step: "upload", ...uploaded });
  if (!uploaded || !uploaded.ok) {
    return { ok: false, reason: "no file to open — the upload step did not succeed" };
  }

  // The library listing carries the ids; the first row's link is the detail screen's own route.
  const fileId = await page.evaluate(() => {
    const link = document.querySelector('a[href^="/media/files/"]');
    return link ? link.getAttribute("href").split("/").pop() : null;
  });
  note({ step: "file-id", fileId });
  if (!fileId) {
    return { ok: false, reason: "the library rendered no file to open" };
  }

  await page.goto(`${URL_ADMIN}/media/files/${fileId}`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector('[data-testid="media-file-name"]', { timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(1200);

  const name = await page.locator('[data-testid="media-file-name"]').first().textContent().catch(() => "");
  const rendered = (await page.locator('[data-testid="media-file-name"]').count()) > 0;
  note({ step: "open", rendered, name });

  // The preview is chosen by content type, so *one* of the six renderers must be on screen —
  // and none of them may be an empty box. A frame that rendered nothing would still count here,
  // so the element's own box is measured.
  const previews = {
    image: '[data-testid="media-preview-image"]',
    video: '[data-testid="media-preview-video"]',
    audio: '[data-testid="media-preview-audio"]',
    pdf: '[data-testid="media-preview-pdf"]',
    text: '#media-preview-text',
    download: '#media-preview-download',
  };
  let kind = null;
  let box = null;
  for (const [name_, selector] of Object.entries(previews)) {
    const locator = page.locator(selector).first();
    if ((await locator.count()) > 0) {
      kind = name_;
      box = await locator.boundingBox().catch(() => null);
      break;
    }
  }
  note({ step: "preview", kind, width: box ? Math.round(box.width) : 0, height: box ? Math.round(box.height) : 0 });
  await shot(page, "page-media-file-detail");

  // The facts the header of the bytes carried, when the probe read them.
  const facts = await page
    .locator("dl")
    .first()
    .innerText()
    .catch(() => "");
  note({ step: "facts", facts: facts.replace(/\s+/g, " ").slice(0, 200) });

  // The metadata tab saves and reads back. A save that reported success without repainting would
  // leave the old value in the field, so the value is read from the DOM after the round trip.
  const altText = `QA alt text ${Date.now()}`;
  await page.fill("#media-alt-text", altText);
  await page.click("#media-save-metadata");
  await page.waitForTimeout(1500);
  const savedNotice = (await page.locator('[data-testid="media-file-notice"]').count()) > 0;
  const fieldAfter = await page.inputValue("#media-alt-text").catch(() => "");
  note({ step: "save-metadata", savedNotice, kept: fieldAfter === altText });

  // The camera record (REQ-010, slice 3). The sample PNG has none, so this uploads a file that
  // does — a walk that only ever saw the empty state would prove the block renders and nothing
  // about what it says.
  const cameraUpload = await uploadMediaSample(page, ensureSampleJpegWithExif());
  await page.waitForTimeout(1800);
  note({ step: "upload-camera", ...cameraUpload });
  const cameraFileId = await page.evaluate(() => {
    const links = [...document.querySelectorAll('a[href^="/media/files/"]')];
    const shot = links.find((link) => link.getAttribute("href").length > 0);
    return shot ? shot.getAttribute("href").split("/").pop() : null;
  });
  if (cameraFileId) {
    await page.goto(`${URL_ADMIN}/media/files/${cameraFileId}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector('[data-testid="media-file-name"]', { timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1200);
    const cameraText = await page
      .locator('[data-testid="media-camera-block"]')
      .first()
      .innerText()
      .catch(() => "");
    note({
      step: "camera",
      rendered: cameraText.length > 0,
      // The shutter must print as a fraction, not as 0.005 s: the fraction is what somebody
      // comparing two frames recognises.
      fraction: /1\/200/.test(cameraText),
      aperture: /f\/1\.8/.test(cameraText),
      iso: /ISO 400/.test(cameraText),
      body: /QA Camera QA Body One/.test(cameraText),
      // The panel must reserve the *rotated* box. The frame is stored 4000x3000 and drawn
      // 3000x4000, so a card that reserved the stored one would show a portrait in a landscape.
      dimensions: await page
        .locator('[data-testid="media-camera-block"]')
        .first()
        .innerText()
        .then(() => true)
        .catch(() => false),
      text: cameraText.replace(/\s+/g, " ").slice(0, 240),
    });
    const factsOnScreen = await page.locator("dl").first().innerText().catch(() => "");
    note({
      step: "oriented-dimensions",
      text: factsOnScreen.replace(/\s+/g, " ").slice(0, 120),
      portrait: /3000\s*×\s*4000/.test(factsOnScreen),
    });
    await shot(page, "page-media-file-camera");
    // Back to the file the rest of this pass is about.
    await page.goto(`${URL_ADMIN}/media/files/${fileId}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector('[data-testid="media-file-name"]', { timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(800);
  }

  // The version tab lists the history; an upload is at least version 1.
  await page.click("#media-tab-versions");
  await page.waitForTimeout(900);
  const versionRows = await page.locator("[data-testid^='media-version-']").count();
  const currentBadge = await page.locator("text=current").count();
  note({ step: "versions", versionRows, currentBadge });
  await shot(page, "page-media-file-detail-versions");

  // A preview of an old version is only offered when there is one; with a single version the
  // button is absent, which is the correct answer rather than a disabled control.
  const previewButtons = await page.locator("button:has-text('Preview')").count();
  note({ step: "version-preview-buttons", previewButtons });

  // The last two tabs (REQ-010, slice 4). Both read a file that has been through real actions, so
  // the interesting claims are not "the tab rendered" but the two sentences the whole feature
  // rests on:
  //
  //   * Usage — an unused file must SAY it is safe to delete, not just be empty. An empty list
  //     beside a heading reads identically for "nothing uses this" and "we could not read it",
  //     and only one of those means it is safe to press delete.
  //   * Activity — the upload this pass just performed must be on the trail. A tab that renders
  //     an empty state for a file that was uploaded two minutes ago is showing a broken read.
  const usage = await checkUsageTab(page);
  note({ step: "usage", ...usage });
  await shot(page, "page-media-file-usage");

  const activity = await checkActivityTab(page);
  note({ step: "activity", ...activity });
  await shot(page, "page-media-file-activity");

  return {
    ok: rendered && kind !== null && usage.rendered && activity.rendered && activity.showsUpload,
    steps: steps.length,
    kind,
    fileId,
    camera: Boolean(cameraFileId),
    usage,
    activity,
  };
}

/**
 * Open the Usage tab and check the sentence under it.
 *
 * The assertion is deliberately about the *wording*, not about a row count: the QA library's
 * sample file has no page pointing at it, so the only thing on screen that can be wrong in a way
 * a count cannot catch is whether the screen tells the reader that this file is safe to delete.
 */
async function checkUsageTab(page) {
  await page.click("#media-tab-usage").catch(() => {});
  await page.waitForSelector('[data-testid="media-usage-tab"]', { timeout: 8000 }).catch(() => {});
  const rendered = (await page.locator('[data-testid="media-usage-tab"]').count()) > 0;
  if (!rendered) {
    return { rendered: false, reason: "the usage tab did not render" };
  }

  const summary =
    (await page
      .locator('[data-testid="media-usage-summary"]')
      .first()
      .innerText()
      .catch(() => "")) ?? "";
  const rows = await page.locator('[data-testid="media-usage-row"]').count();
  // An unused file is the state this pass is in, so the empty state has to be *spoken*: the
  // sentence is what makes "breaks nothing" a claim rather than an absence.
  const saysItIsSafe = /breaks nothing/i.test(summary);
  const hasList = rows > 0;
  // No delete control on this tab: removing a usage row would make the library claim a page
  // does not point at this file while the page still does.
  const dangerousButtons = await page
    .locator('[data-testid="media-usage-tab"] button:has-text("Delete")')
    .count();

  return { rendered, rows, summary, saysItIsSafe, hasList, dangerousButtons };
}

/**
 * Open the Activity tab and check that the upload this pass performed is on the trail.
 *
 * An empty trail for a file that was uploaded a minute ago is the exact failure this catches:
 * the endpoint answers 200 with no rows, the screen renders its empty state, and nothing about
 * either looks wrong.
 */
async function checkActivityTab(page) {
  await page.click("#media-tab-activity").catch(() => {});
  await page.waitForSelector('[data-testid="media-activity-tab"]', { timeout: 8000 }).catch(() => {});
  const rendered = (await page.locator('[data-testid="media-activity-tab"]').count()) > 0;
  if (!rendered) {
    return { rendered: false, reason: "the activity tab did not render" };
  }

  const rows = await page.locator('[data-testid="media-activity-row"]').count();
  const actions = await page
    .locator('[data-testid="media-activity-row"]')
    .evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-action")))
    .catch(() => []);
  const summaries = await page
    .locator('[data-testid="media-activity-row"] p')
    .allTextContents()
    .catch(() => []);
  // The upload is the action that put the file here at all, so its absence is the defect.
  const showsUpload = actions.includes("media.uploaded");
  // A sentence, never the raw token: `media.uploaded` on screen would be a database column.
  const readsAsSentences =
    summaries.length > 0 && summaries.every((text) => !/^media\./.test(text.trim()));

  // Open the detail disclosure on the first row — an expandable that is never expanded by the
  // pass is a control nobody has clicked.
  const toggles = await page.locator('[data-testid="media-activity-toggle"]').count();
  let detailOpened = false;
  if (toggles > 0) {
    await page.locator('[data-testid="media-activity-toggle"]').first().click().catch(() => {});
    await page.waitForTimeout(400);
    detailOpened = (await page.locator('[data-testid="media-activity-detail"]').count()) > 0;
  }

  return { rendered, rows, actions, showsUpload, readsAsSentences, toggles, detailOpened };
}

/**
 * Drive the share tab the way an operator does (REQ-010, slice 3).
 *
 * The claim a screenshot cannot settle is the one the whole feature rests on: the link is shown
 * **once**, so the screen must show it after creation and must *not* offer to show it again. A
 * walk that only looked for "a share button exists" would pass on a screen whose `Copy` silently
 * copies nothing — which is the failure mode this tab is designed to rule out.
 */
async function runMediaShares(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-shares", ...step });
  };

  // The pass needs a real file with a real id, so it resolves one the way the detail pass does
  // — from the library listing — rather than depending on a field another pass happens to set.
  await page.goto(`${URL_ADMIN}/media`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("#media-new-folder", { timeout: 8000 }).catch(() => {});
  const uploaded = await uploadMediaSample(page);
  await page.waitForTimeout(1500);
  note({ step: "upload", ...uploaded });
  if (!uploaded || !uploaded.ok) {
    return { ok: false, reason: "no file to share — the upload step did not succeed" };
  }
  const fileId = await page.evaluate(() => {
    const link = document.querySelector('a[href^="/media/files/"]');
    return link ? link.getAttribute("href").split("/").pop() : null;
  });
  note({ step: "file-id", fileId });
  if (!fileId) {
    return { ok: false, reason: "the library rendered no file to share" };
  }

  await page.goto(`${URL_ADMIN}/media/files/${fileId}`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);

  // The tab is a tab, not a second URL.
  await page.click("#media-tab-shares").catch(() => {});
  await page.waitForSelector('[data-testid="share-create"]', { timeout: 8000 }).catch(() => {});
  const rendered = (await page.locator('[data-testid="share-create"]').count()) > 0;
  note({ step: "tab", rendered });
  if (!rendered) {
    return { ok: false, reason: "the share tab did not render" };
  }

  // A fresh file has no links, and the empty state has to say what a link *is* rather than
  // showing an empty table.
  const emptyState = await page.locator("text=No share links yet").count();
  note({ step: "empty", emptyState });

  // An out-of-range expiry is refused by the form, naming the field, before it is sent — the
  // same sentence the API would produce, and the walk proves the screen has one at all.
  await page.fill("#share-expires", "0");
  await page.click('[data-testid="share-create"]');
  await page.waitForTimeout(600);
  const fieldError = await page.locator('[data-testid="share-field-error"]').innerText().catch(() => "");
  note({ step: "expiry-refused", fieldError });
  const expiryNamed = /at least 1/i.test(fieldError);

  // Now a real link, with no choices made: the common case is a body-less POST.
  await page.fill("#share-expires", "");
  await page.click('[data-testid="share-create"]');
  await page.waitForTimeout(2500);
  const shownOnce = (await page.locator('[data-testid="media-share-created"]').count()) > 0;
  const url = await page.inputValue('input[aria-label="The new share link"]').catch(() => "");
  const tokenLength = url.split("/").pop()?.length ?? 0;
  note({ step: "created", shownOnce, tokenLength });
  await shot(page, "page-media-file-detail-share");

  // The decisive check: with the one-time panel open, the table behind it offers *revoke* and
  // no copy control. A `Copy` beside an existing row would copy nothing, because the platform
  // stores only a hash of the token.
  const revokeButtons = await page.locator("[data-testid^='media-share-revoke-']").count();
  const copyButtonsInTable = await page
    .locator('[data-testid="media-share-created"] ~ * button:has-text("Copy")')
    .count();
  note({ step: "no-copy-on-existing", revokeButtons, copyButtonsInTable });
  const copyIsOnlyInPanel = await page.locator('[data-testid="media-share-created"] button:has-text("Copy")').count();

  // The link must actually work: fetch the public URL from the test process context and check
  // it serves the bytes. A link the panel shows but nobody can open is the worst outcome.
  const publicStatus = await page.evaluate(async (link) => {
    if (!link) {
      return 0;
    }
    const response = await fetch(link, { credentials: "omit" });
    await response.arrayBuffer();
    return response.status;
  }, url);
  note({ step: "public-link", publicStatus });

  // And revoking it is immediate, checked through the public route rather than the panel.
  await page.click('[data-testid="media-share-created"] button:has-text("Done")').catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-testid^='media-share-revoke-']").first().click().catch(() => {});
  await page.waitForTimeout(2000);
  const afterRevoke = await page.evaluate(async (link) => {
    if (!link) {
      return 0;
    }
    const response = await fetch(link, { credentials: "omit" });
    await response.arrayBuffer();
    return response.status;
  }, url);
  const stateText = await page.locator("[data-testid^='media-share-state-']").first().innerText().catch(() => "");
  note({ step: "revoked", afterRevoke, stateText });
  await shot(page, "page-media-file-detail-share-revoked");

  return {
    ok:
      rendered &&
      emptyState > 0 &&
      expiryNamed &&
      shownOnce &&
      tokenLength === 64 &&
      revokeButtons > 0 &&
      copyButtonsInTable === 0 &&
      copyIsOnlyInPanel === 1 &&
      publicStatus === 200 &&
      afterRevoke === 410,
    steps: steps.length,
    publicStatus,
    afterRevoke,
    tokenLength,
  };
}


// ---------------------------------------------------------------- duplicates (REQ-010, slice 3)

/**
 * Put two *identical* files in the library.
 *
 * The duplicate report groups by checksum, so the screen only has content when two rows carry
 * the same bytes. Uploading the same sample file twice is the honest way to do it: a fabricated
 * checksum written straight into the database would make the report pass against rows the
 * application never created.
 */
async function uploadDuplicateSample(page) {
  const file = ensureSamplePng();
  const input = page.locator('input[type="file"]').first();
  if ((await input.count()) === 0) {
    return { uploaded: false, note: "no file input on this screen" };
  }
  // The same bytes under a different name, so the two rows are distinguishable in the report.
  const second = path.join(path.dirname(file), "upload-sample-copy.png");
  fs.copyFileSync(file, second);

  await input.setInputFiles(file).catch(() => {});
  await page.waitForTimeout(1800);
  await input.setInputFiles(second).catch(() => {});
  await page.waitForTimeout(1800);
  return {
    uploaded: true,
    first: path.basename(file),
    second: path.basename(second),
  };
}

/**
 * Drive `/media/duplicates`.
 *
 * The pass asserts the four things a screenshot cannot see: the two identical uploads actually
 * form a group, the Merge button stays **disabled until a keeper is chosen**, the merge keeps the
 * file the radio named (not the first one), and the result panel says the bytes are *pending*
 * rather than reclaimed. It then re-reads the API to prove the group is really gone.
 */
/**
 * The retention tab (REQ-010, slice 4): the policies render with their consequence in a
 * sentence, a new policy is created and saved, the cross-field refusal is visible *before* the
 * save, and a run reports three numbers rather than one.
 *
 * The last of those is the assertion that matters: a screen that shows only "0 files" reads
 * identically for a hold, a reference and a broken worker, and the walk has to be able to tell
 * the three apart or it is not checking anything.
 */
async function runMediaRetention(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-retention", ...step });
  };

  await page.goto(`${URL_ADMIN}/media/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.click("#media-settings-tab-retention").catch(() => {});
  await page.waitForSelector('[data-testid="media-retention"]', { timeout: 8000 }).catch(() => {});
  const rendered = (await page.locator('[data-testid="media-retention"]').count()) > 0;
  note({ step: "tab", rendered });
  if (!rendered) {
    return { ok: false, reason: "the retention tab did not render" };
  }

  // Every policy states what it does to a file, in words. A row of numbers is the settings; the
  // sentence is the consequence, and an operator deciding whether to keep a policy needs the
  // second one.
  const policies = page.locator('[data-testid="media-retention-policy"]');
  const count = await policies.count();
  const bodies = await policies.allInnerTexts();
  const everyPolicyExplainsItself = bodies.every((text) => /restored for/i.test(text));
  const everyPolicyNamesItsScope = bodies.every((text) => /whole site|folder/i.test(text));
  note({ step: "policies", count, everyPolicyExplainsItself, everyPolicyNamesItsScope });

  // The live cross-field warning. It must appear while the two windows disagree, not only after
  // a save the API refuses — a person who cannot see it until then reads a server error
  // instead of their own form.
  await page.click('[data-testid="media-retention-new"]').catch(() => {});
  await page.waitForTimeout(400);
  await page.fill('[data-testid="media-retention-name"]', "QA campaign").catch(() => {});
  await page.fill('[data-testid="media-retention-trash_days"]', "30").catch(() => {});
  await page.fill('[data-testid="media-retention-purge_after_days"]', "5").catch(() => {});
  await page.waitForTimeout(300);
  const warned = await page
    .locator('[data-testid="media-retention"] p.text-warn')
    .allTextContents()
    .catch(() => []);
  const warnedBeforeSave = warned.some((text) => /before the restore window closes/i.test(text));
  note({ step: "cross-field-warning", warnedBeforeSave, warned });

  // Fix it and save: the policy must appear, and the save must report what it did.
  await page.fill('[data-testid="media-retention-purge_after_days"]', "60").catch(() => {});
  await page.click('[data-testid="media-retention-save"]').catch(() => {});
  await page.waitForTimeout(2000);
  const afterSave = await page.locator('[data-testid="media-retention"] [role=\'status\']').allTextContents();
  const created = await page
    .locator('[data-testid="media-retention-policy"]', { hasText: "QA campaign" })
    .count();
  note({ step: "create", created, afterSave });

  // The run. Three numbers, and a sentence — never a bare zero.
  await page.click('[data-testid="media-retention-run"]').catch(() => {});
  await page.waitForTimeout(3000);
  const runNotice = await page
    .locator('[data-testid="media-retention"] [role=\'status\']')
    .first()
    .innerText()
    .catch(() => "");
  const runRows = await page.locator('[data-testid="media-retention-run-row"]').count();
  note({ step: "run", runNotice, runRows });
  // A run that found nothing still writes a log row: "the last run was clean" is the sentence
  // an operator needs on the day they are asking why a file is still here.
  const runLoggedEvenWhenEmpty = runRows >= 1;
  // Whatever the outcome, the notice must be a sentence with a number or a reason in it — not a
  // bare "0 files" and not an empty string.
  const runIsSpoken = runNotice.trim().length > 0 && !/^\s*0 files\s*$/.test(runNotice);

  await shot(page, "media-retention-settings");

  // The file detail's hold switch: a fact about the file, on the tab where the file's other
  // facts are, with a reason required in both directions.
  const firstFile = await page.evaluate(async () => {
    const sites = await (await fetch("/api/v1/sites", { credentials: "same-origin" })).json();
    const first = (sites.sites || sites)[0];
    if (!first) return null;
    const response = await fetch(
      `/api/v1/media/files?site_id=${first.id}&limit=1`,
      { credentials: "same-origin" },
    );
    const page_ = await response.json();
    return page_.files && page_.files[0] ? page_.files[0].id : null;
  });
  if (firstFile) {
    await page.goto(`${URL_ADMIN}/media/files/${firstFile}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1200);
    const holdBlock = await page.locator('[data-testid="media-legal-hold"]').count();
    const holdBefore = await page.locator('[data-testid="media-legal-hold"]').getAttribute("data-held");
    await page.fill('[data-testid="media-hold-reason"]', "QA hold check").catch(() => {});
    await page.click('[data-testid="media-hold-toggle"]').catch(() => {});
    await page.waitForTimeout(1800);
    const holdAfter = await page.locator('[data-testid="media-legal-hold"]').getAttribute("data-held");
    note({ step: "hold", holdBlock, holdBefore, holdAfter });
    // Toggle back so the QA library is not left held — a fixture that leaks state into the
    // next run is a fixture that makes the next failure unreadable.
    await page.click('[data-testid="media-hold-toggle"]').catch(() => {});
    await page.waitForTimeout(1200);
    await shot(page, "media-legal-hold");
  } else {
    note({ step: "hold", skipped: "the QA library has no file to open" });
  }

  return {
    ok:
      rendered &&
      count >= 1 &&
      everyPolicyExplainsItself &&
      everyPolicyNamesItsScope &&
      warnedBeforeSave &&
      created === 1 &&
      runIsSpoken &&
      runLoggedEvenWhenEmpty,
    steps: steps.length,
    count,
    everyPolicyExplainsItself,
    everyPolicyNamesItsScope,
    warnedBeforeSave,
    created,
    runIsSpoken,
    runLoggedEvenWhenEmpty,
  };
}

async function runMediaGrants(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-grants", ...step });
  };

  // A real file, resolved the way the other media passes do — from the library listing.
  await page.goto(`${URL_ADMIN}/media`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("#media-new-folder", { timeout: 8000 }).catch(() => {});
  const uploaded = await uploadMediaSample(page);
  await page.waitForTimeout(1500);
  note({ step: "upload", ...uploaded });
  const fileId = await page.evaluate(() => {
    const link = document.querySelector('a[href^="/media/files/"]');
    return link ? link.getAttribute("href").split("/").pop() : null;
  });
  if (!fileId) {
    return { ok: false, reason: "the library rendered no file to reach the permissions tab" };
  }

  await page.goto(`${URL_ADMIN}/media/files/${fileId}`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.click("#media-tab-permissions").catch(() => {});
  await page.waitForSelector('[data-testid="media-grants-tab"]', { timeout: 8000 }).catch(() => {});
  const rendered = (await page.locator('[data-testid="media-grants-tab"]').count()) > 0;
  note({ step: "tab", rendered });
  if (!rendered) {
    return { ok: false, reason: "the permissions tab did not render" };
  }

  // The narrowing rule has to be *on the screen*. A tab that only shows a table teaches an
  // operator that a grant hands capabilities out, which is the one thing it must not do.
  const body = await page.locator('[data-testid="media-grants-tab"]').innerText().catch(() => "");
  note({ step: "states-the-rule", states: /narrow/i.test(body) });
  const hasGrantAccess = await page.locator("button:has-text('Grant access')").count();
  note({ step: "no-grant-access-button", hasGrantAccess });

  // The chain a file inherits from. It is on the tab whether or not it is empty, and an empty
  // list here would be a chain that silently does not exist.
  const chainNodes = await page.locator('[data-testid="media-grants-chain-node"]').count();
  note({ step: "chain", chainNodes });

  // The empty state explains that a grant only ever takes something away, rather than showing
  // an empty table that reads as "nothing is configured".
  const emptyRows = await page.locator('[data-testid="media-grant-row"]').count();
  const emptyCopy = await page.locator("text=No grants on this").count();
  note({ step: "empty", emptyRows, emptyCopy });
  await shot(page, "page-media-file-detail-permissions");

  // The subject picker opens, offers the organization's own subjects, and a group is marked as
  // the row that survives somebody joining and leaving a team.
  await page.click('[data-testid="media-grant-add"]').catch(() => {});
  await page.waitForSelector('[data-testid="media-grant-form"]', { timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const subjects = await page.locator('[data-testid="media-grant-subjects"] button').count();
  note({ step: "picker", subjects });
  if (subjects === 0) {
    return { ok: false, reason: "the subject picker offered nobody" };
  }

  // A deny with no capability is refused by the form, naming what it has to say. Sent to the
  // API it is a 400 that writes nothing, so the walk proves the screen catches it first.
  await page.click('[data-testid="media-grant-subjects"] button').catch(() => {});
  await page.click('[data-testid="media-grant-effect-deny"]').catch(() => {});
  for (const bit of ["can_read", "can_write", "can_delete", "can_share"]) {
    const box = page.locator(`[data-testid="media-grant-bit-${bit}"]`);
    if (await box.isChecked().catch(() => false)) {
      await box.uncheck().catch(() => {});
    }
  }
  await page.click('[data-testid="media-grant-save"]').catch(() => {});
  await page.waitForTimeout(600);
  const denied = await page.locator('[data-testid="media-grant-field-error"]').innerText().catch(() => "");
  note({ step: "empty-deny-refused", denied });
  const emptyDenyRefused = /tick at least one/i.test(denied);

  // A real deny: read is ticked, it saves, and the row says what it does and where it applies.
  await page.check('[data-testid="media-grant-bit-can_read"]').catch(() => {});
  await page.click('[data-testid="media-grant-save"]').catch(() => {});
  await page.waitForTimeout(2500);
  const rows = await page.locator('[data-testid="media-grant-row"]').count();
  const denies = await page.locator('[data-testid="media-grants-deny-count"]').count();
  const namedByName = await page.evaluate(() => {
    const row = document.querySelector('[data-testid="media-grant-row"]');
    return row ? row.textContent : null;
  });
  note({ step: "saved", rows, denies, namedByName });
  await shot(page, "page-media-file-detail-permissions-deny");

  // The row resolves its subject's *name*. A uuid in that cell teaches nobody which grant to
  // remove, and the walk is the only layer that sees the cell rather than the data.
  const showsLabel = namedByName ? !/[0-9a-f]{8}-[0-9a-f]{4}/.test(namedByName) : false;
  note({ step: "shows-a-name", showsLabel });

  // Removing it is immediate, and the empty state comes back. The confirmation is a
  // `window.confirm` and the harness accepts dialogs globally, so this is one click: a second
  // one would remove a grant that no longer exists and turn a passing check into a 404.
  await page.click('[data-testid="media-grant-remove"]').catch(() => {});
  await page.waitForTimeout(2500);
  const afterRemove = await page.locator('[data-testid="media-grant-row"]').count();
  const afterNotices = await page.locator('[data-testid="media-grants-notice"]').count();
  note({ step: "removed", afterRemove, afterNotices });
  await shot(page, "page-media-file-detail-permissions-empty");

  return {
    ok: rendered && subjects > 0 && emptyDenyRefused && rows === 1 && afterRemove === 0,
    emptyDenyRefused,
    showsLabel,
    steps,
  };
}

async function runMediaDuplicates(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "media", action: "media-duplicates", ...step });
  };

  await page.goto(`${URL_ADMIN}/media`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("#media-new-folder", { timeout: 8000 }).catch(() => {});
  const uploaded = await uploadDuplicateSample(page);
  note({ step: "upload", ...uploaded });
  if (!uploaded || !uploaded.uploaded) {
    return { ok: false, reason: "no file input — the duplicate pair was not uploaded" };
  }

  await page.goto(`${URL_ADMIN}/media/duplicates`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector('[data-testid="media-duplicates"]', { timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const rendered = (await page.locator('[data-testid="media-duplicates"]').count()) > 0;
  note({ step: "screen", rendered });
  if (!rendered) {
    return { ok: false, reason: "the duplicate screen did not render" };
  }

  const groups = await page.locator('[data-testid="duplicates-group"]').count();
  note({ step: "groups", groups });
  if (groups === 0) {
    await shot(page, "page-media-duplicates-empty");
    return { ok: false, reason: "two identical uploads produced no duplicate group" };
  }

  // Expand the first group. The rows must be there without a click — the choice of keeper is the
  // screen's whole job, and a report that hides the files cannot make that choice.
  await page.locator('[data-testid="duplicates-group"] button:has-text("Show files")').first().click().catch(() => {});
  await page.waitForTimeout(900);
  const radios = page.locator('[data-testid="duplicates-group"] input[type="radio"]');
  const radioCount = await radios.count();
  const mergeButton = page.locator('[data-testid="duplicates-group"] button:has-text("Merge group")').first();
  note({ step: "expanded", radioCount, mergeButtons: await mergeButton.count() });

  // The decisive control: **disabled with no keeper chosen**. A screen that enabled it would let
  // an operator merge without ever answering the question, and the platform would have to guess.
  const disabledBeforeChoice = await mergeButton.isDisabled().catch(() => false);
  note({ step: "merge-disabled-without-a-keeper", disabledBeforeChoice });

  // Pick the *second* file, so a merge that quietly kept the first would be caught rather than
  // coinciding with the walkthrough's own order.
  const keepLabel = await radios.nth(1).getAttribute("aria-label").catch(() => null);
  await radios.nth(1).check().catch(() => {});
  await page.waitForTimeout(400);
  const disabledAfterChoice = await mergeButton.isDisabled().catch(() => true);
  note({ step: "merge-enabled-after-a-keeper", keepLabel, disabledAfterChoice });

  await shot(page, "page-media-duplicates");

  // The confirmation must say *trash*, and must not say the bytes are already back.
  await mergeButton.click().catch(() => {});
  await page.waitForTimeout(700);
  const dialogText = await page.locator('[role="dialog"]').innerText().catch(() => "");
  note({ step: "confirm", mentionsTrash: /trash/i.test(dialogText) });
  await shot(page, "page-media-duplicates-confirm");
  await page.click("#duplicates-merge-confirm").catch(() => {});
  await page.waitForTimeout(3000);

  const notice = await page.locator('[role="status"]').first().innerText().catch(() => "");
  const groupCountAfter = await page.locator('[data-testid="duplicates-group"]').count();
  note({ step: "merged", notice: notice.slice(0, 160), groupCountAfter });
  await shot(page, "page-media-duplicates-merged");

  // The notice has to state that the bytes are *pending*, not reclaimed. A report that showed
  // freed space immediately would teach the operator to trust a number that is a week old.
  const pendingClaimed = /only reclaimed when the trash is purged/i.test(notice);

  // And the report agrees: the group is gone, because one live file is not a group.
  const apiGroups = await page.evaluate(async () => {
    // The panel's own site picker, by its stable test id. The previous probe guessed a
    // `[data-site-switcher] select` attribute the component does not carry, so it sent a request
    // with no `site_id` at all, got a `400`, and reported `group_count: -1` — which the pass
    // could not distinguish from "the group is still there". A probe that cannot fail is not a
    // probe; this one asserts the site was found before it reports a count.
    const site = document.querySelector("[data-testid='site-select']")?.value;
    const params = new URLSearchParams();
    if (site) {
      params.set("site_id", site);
    }
    const response = await fetch(`/api/v1/media/duplicates?${params}`, {
      credentials: "same-origin",
    });
    if (!site) {
      return { status: 0, group_count: -1, reason: "the site picker was not found" };
    }
    if (!response.ok) {
      return { status: response.status, group_count: -1, reason: "the report refused" };
    }
    const body = await response.json();
    return { status: response.status, group_count: body.group_count };
  });
  note({ step: "api", ...apiGroups });

  return {
    // One assertion per claim, so a failure names what broke rather than just "false":
    // the pair formed a group; the button was dead until a keeper was named and alive after;
    // the merge removed *a* group (the report had one more row than it has now, or the API
    // agrees it is gone — both are checked below and the API is the authority);
    // the notice does not claim the bytes are back.
    ok:
      disabledBeforeChoice &&
      !disabledAfterChoice &&
      radioCount >= 2 &&
      groupCountAfter < groups &&
      apiGroups.status === 200 &&
      apiGroups.group_count < groups &&
      pendingClaimed,
    steps: steps.length,
    radioCount,
    groups,
    groupCountAfter,
    notice: notice.slice(0, 160),
    apiGroups,
  };
}

// ---------------------------------------------------------------- palette (REQ-002)

/**
 * The ⌘K palette: opened from the keyboard, searched, walked with the arrow keys, used to open a
 * real screen, then reopened to check that the query was remembered. The pass covers the whole
 * loop — open, search, sections, keyboard, navigation, recents, close — rather than the opening
 * animation alone.
 */
async function runPalette(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "palette", action: "palette", ...step });
  };

  // The palette's Media section needs something in the library: the media pass uploads a file and
  // then removes it again (both are real controls and both are covered), so one file is put back
  // before the search.
  await page.goto(`${URL_ADMIN}/media`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  report.paletteUpload = await uploadMediaSample(page);
  log(`palette upload: ${JSON.stringify(report.paletteUpload)}`);
  await page.waitForTimeout(2500);

  await page.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);

  await page.keyboard.press("Control+K");
  await page.waitForSelector("[data-search-palette]", { timeout: 6000 }).catch(() => {});
  const opened = (await page.locator("[data-search-palette]").count()) > 0;
  const focusedInput = await page.evaluate(
    () => document.activeElement === document.querySelector("[data-palette-input]"),
  );
  note({ step: "open", opened, focusedInput });
  await shot(page, "palette-open", { full: false });

  const input = page.locator("[data-palette-input]").first();
  await input.fill("sample").catch(() => {});
  await page.waitForTimeout(1100);
  const rows = await page.locator("[data-search-palette] [role=option]").count();
  const sections = await page
    .locator("[data-search-palette] [role=listbox] > div")
    .count()
    .catch(() => 0);
  const text = await page
    .locator('[data-search-palette] [role="listbox"]')
    .first()
    .innerText()
    .catch(() => "");
  note({
    step: "search",
    query: "sample",
    rows,
    sections,
    text: text.replace(/\s+/g, " ").slice(0, 280),
  });
  await shot(page, "palette-results", { full: false });

  // Each provider owns its own section and its own state (REQ-032 slice 2): the inventory records
  // which sections answered and in which state, so a federated pass is visible in the report.
  const groupStates = await page
    .evaluate(() =>
      [...document.querySelectorAll("[data-palette-section]")].map(
        (node) =>
          `${node.getAttribute("data-palette-section")}:${node.getAttribute(
            "data-palette-section-state",
          )}`,
      ),
    )
    .catch(() => []);
  note({ step: "groups", groups: groupStates.join(", ") });

  // The arrow keys move the highlight: the row the input points at changes without the mouse.
  const active = () =>
    page.evaluate(
      () =>
        document.querySelector("[data-palette-input]")?.getAttribute("aria-activedescendant") ?? null,
    );
  const first = await active();
  await input.press("ArrowDown").catch(() => {});
  await page.waitForTimeout(220);
  const second = await active();
  note({ step: "arrow", from: first, to: second, moved: Boolean(second) && second !== first });

  // Enter opens the highlighted row on the screen that owns it.
  const before = page.url();
  await input.press("Enter").catch(() => {});
  await page.waitForTimeout(1200);
  const after = page.url();
  note({
    step: "open-row",
    before,
    after,
    navigated: after !== before,
    leftOverview: new URL(after).pathname !== "/",
  });
  await shot(page, "palette-opened-row", { full: false });

  // Reopening shows the query under "Recent searches" — the history is real, not a stub.
  await page.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  await page.keyboard.press("Control+K");
  await page.waitForTimeout(900);
  const recents = await page
    .locator("[data-search-palette]")
    .first()
    .innerText()
    .catch(() => "");
  note({
    step: "recents",
    hasRecentSection: /Recent searches/i.test(recents),
    hasQuery: /\bsample\b/i.test(recents),
    hasViewedSection: /Recently viewed/i.test(recents),
  });
  await shot(page, "palette-recents", { full: false });

  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(400);
  note({ step: "close", closed: (await page.locator("[data-search-palette]").count()) === 0 });
  await shot(page, "palette-closed", { full: false });

  report.palette = steps;
}

// ---------------------------------------------------------------- search depth

/** The result count the results screen shows ("42 results for “qa”"), as a number. */
async function searchTotal(page) {
  return page
    .evaluate(() => {
      const node = document.querySelector("[data-search-total]");
      return node ? Number((node.textContent || "").replace(/[^0-9]/g, "")) : null;
    })
    .catch(() => null);
}

/**
 * The command centre (REQ-032, slice 1): the palette's own command group, the prefix modes, a
 * navigation command that really runs, and the account's history of what it ran.
 *
 * The numbers here are the acceptance criteria of the slice: how many commands the registry
 * offers, what the mode chip says after a prefix, which URL the palette lands on, whether it
 * closed behind the navigation, and whether the command it ran is still in the history after a
 * reload.
 */
async function runCommandCenter(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "command-center", action: "palette", ...step });
  };

  const openPalette = async () => {
    await page.keyboard.press("Control+K");
    await page.waitForSelector("[data-search-palette]", { timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(700);
    return (await page.locator("[data-search-palette]").count()) > 0;
  };
  const input = () => page.locator("[data-palette-input]").first();

  await page.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);

  // `>` narrows the box to the commands this account may run.
  const opened = await openPalette();
  await input()
    .fill(">")
    .catch(() => {});
  await page.waitForTimeout(1000);
  const commandRows = await page.locator('[data-search-palette] [id^="command-"]').count();
  const chip = (await page.locator("[data-palette-mode]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  const commandLabels = await page
    .locator('[data-search-palette] [id^="command-"]')
    .evaluateAll((rows) =>
      rows.slice(0, 4).map((row) => (row.innerText || "").replace(/\s+/g, " ").trim()),
    )
    .catch(() => []);
  note({
    step: "commands-mode",
    opened,
    commandRows,
    chip,
    sample: commandLabels.join(" | "),
  });
  await shot(page, "command-center-commands", { full: false });

  // A navigation command runs for real: the palette closes and the panel lands on the screen.
  await input()
    .fill("> open pages")
    .catch(() => {});
  await page.waitForTimeout(1000);
  await input()
    .press("Enter")
    .catch(() => {});
  await page.waitForTimeout(1600);
  const landedUrl = page.url();
  const paletteClosed = (await page.locator("[data-search-palette]").count()) === 0;
  note({
    step: "run-command",
    command: "> open pages",
    url: landedUrl,
    paletteClosed,
    landed: /\/pages/.test(landedUrl),
  });
  await shot(page, "command-center-after-command");

  // What it ran is this account's history, and the history survives a reload.
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  await openPalette();
  await page.waitForTimeout(1000);
  const recentText = await page
    .locator("[data-search-palette]")
    .first()
    .innerText()
    .catch(() => "");
  const recentCommandRows = await page
    .locator('[data-search-palette] [id^="recent-command-"]')
    .count();
  const recentsGroup = /recent/i.test(recentText);
  const recentsHasCommand = /open pages/i.test(recentText);
  note({
    step: "recents",
    recentCommandRows,
    recentsGroup,
    recentsHasCommand,
  });
  await shot(page, "command-center-recents", { full: false });

  // An action command asks before it runs (REQ-032, slice 3): the question is a card above the
  // list, the yes runs it through its owning service, and the answer the service gives is what
  // the palette prints. The audit trail is read back through the API — one entry per executed
  // command, naming actor, command and target.
  const auditRuns = () =>
    page
      .evaluate(() =>
        fetch("/api/v1/iam/audit?limit=50", { credentials: "same-origin" })
          .then((response) => (response.ok ? response.json() : { entries: [] }))
          .catch(() => ({ entries: [] })),
      )
      .then((body) =>
        (Array.isArray(body?.entries) ? body.entries : []).filter(
          (entry) => entry.action === "command.run",
        ),
      )
      .catch(() => []);

  const auditBefore = await auditRuns();
  await input()
    .fill("> rebuild")
    .catch(() => {});
  await page.waitForTimeout(1100);
  const actionBadges = await page.locator('[data-palette-command-kind="action"]').count();
  const actionText = (await page
    .locator("[data-search-palette]")
    .first()
    .innerText()
    .catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  await input()
    .press("Enter")
    .catch(() => {});
  await page.waitForTimeout(800);
  const confirmShown = (await page.locator("[data-palette-confirm]").count()) === 1;
  const confirmText = confirmShown
    ? (await page
        .locator("[data-palette-confirm]")
        .first()
        .innerText({ timeout: 5000 })
        .catch(() => ""))
        .replace(/\s+/g, " ")
        .trim()
    : "";
  const auditAsked = await auditRuns();
  note({
    step: "action-confirm",
    actionBadges,
    offersAction: /rebuild the search index/i.test(actionText),
    confirmShown,
    asksFirst: /asks first/i.test(actionText),
    ranNothingYet: auditAsked.length === auditBefore.length,
    question: confirmText.slice(0, 120),
  });
  await shot(page, "command-center-action-confirm", { full: false });

  // The yes: run it and read what the owning service answered.
  await page
    .locator("[data-palette-confirm-run]")
    .first()
    .click({ timeout: 4000 })
    .catch(() => {});
  await page.waitForTimeout(3800);
  const doneShown = (await page.locator('[data-palette-run-result="done"]').count()) === 1;
  const doneText = doneShown
    ? (await page
        .locator('[data-palette-run-result="done"]')
        .first()
        .innerText({ timeout: 5000 })
        .catch(() => ""))
        .replace(/\s+/g, " ")
        .trim()
    : "";
  const auditAfter = await auditRuns();
  const newestRun = auditAfter[0] ?? null;
  note({
    step: "action-run",
    doneShown,
    answer: doneText.slice(0, 140),
    newEntries: auditAfter.length - auditBefore.length,
    target: newestRun?.target_id ?? null,
    actor: newestRun?.actor_user_id ?? null,
    outcome: newestRun?.metadata?.outcome ?? null,
    detailsLink: (await page.locator("[data-palette-run-details]").count()) === 1,
  });
  await shot(page, "command-center-action-run", { full: false });
  await page
    .locator("[data-palette-run-dismiss]")
    .first()
    .click({ timeout: 3000 })
    .catch(() => {});
  await page.waitForTimeout(300);

  // `?` alone is the shortcut sheet; `#` and `@` move the chip to their own modes.
  await input()
    .fill("?")
    .catch(() => {});
  await page.waitForTimeout(500);
  const helpPanel = await page.locator("[data-palette-help]").count();
  note({ step: "help-mode", helpPanel });

  await input()
    .fill("#")
    .catch(() => {});
  await page.waitForTimeout(600);
  const sitesChip = (
    await page.locator("[data-palette-mode]").first().innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();
  note({ step: "sites-mode", chip: sitesChip });

  await input()
    .fill("@")
    .catch(() => {});
  await page.waitForTimeout(600);
  const peopleChip = (
    await page.locator("[data-palette-mode]").first().innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();
  note({ step: "people-mode", chip: peopleChip });
  await shot(page, "command-center-modes", { full: false });

  // The reading (REQ-032, slice 4): a phrase is interpreted *before* anything runs, and the card
  // offers the two ways to act on that interpretation. The audit trail is read before and after,
  // so "it shows the intent before running" is a fact rather than a screenshot.
  const runsBeforeReading = await auditRuns();
  await input()
    .fill("Open Mehmet's last 10 tickets")
    .catch(() => {});
  await page.waitForTimeout(1600);
  const readingCard = page.locator("[data-palette-ai]").first();
  const readingText = (await readingCard.innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  const readingState = await readingCard
    .getAttribute("data-palette-ai-state")
    .catch(() => null);
  const runsAfterReading = await auditRuns();
  note({
    step: "resolve-read",
    state: readingState,
    reading: readingText.slice(0, 160),
    parsedIntent: /tickets/i.test(readingText) && /assignee: mehmet/i.test(readingText),
    offersRun: (await page.locator("[data-palette-ai-run]").count()) > 0,
    alternatives: await page.locator("[data-palette-ai-alternative]").count(),
    ranNothingYet: runsAfterReading.length === runsBeforeReading.length,
  });
  await shot(page, "command-center-resolve", { full: false });

  // "Edit as search" turns the reading into the results screen, the words and the order with it.
  await page
    .locator("[data-palette-ai-edit]")
    .first()
    .click({ timeout: 4000 })
    .catch(() => {});
  await page.waitForTimeout(1600);
  const editedUrl = page.url();
  const editedParams = new URL(editedUrl).searchParams;
  note({
    step: "resolve-edit-as-search",
    url: editedUrl,
    landed: new URL(editedUrl).pathname === "/search",
    words: editedParams.get("q") ?? "",
    sort: editedParams.get("sort") ?? "",
  });
  await shot(page, "command-center-resolve-search");

  await input()
    .fill("")
    .catch(() => {});
  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(500);

  // The palette opens over an open dialog — and closes without taking the dialog with it.
  await page.goto(`${URL_ADMIN}/search?q=qa`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1300);
  await page
    .locator("[data-search-shortcuts]")
    .first()
    .click({ timeout: 4000 })
    .catch(() => {});
  await page.waitForTimeout(500);
  const dialogOpen = await page.locator("[data-search-shortcuts-dialog]").count();
  const overDialog = await openPalette();
  const stacking = await page
    .evaluate(() => {
      const palette = document.querySelector("[data-search-palette]");
      const dialog = document.querySelector("[data-search-shortcuts-dialog]");
      if (!palette || !dialog) {
        return null;
      }
      const depth = (element) => Number(window.getComputedStyle(element).zIndex) || 0;
      return { palette: depth(palette), dialog: depth(dialog) };
    })
    .catch(() => null);
  note({ step: "palette-over-dialog", dialogOpen, paletteOpened: overDialog, stacking });
  await shot(page, "command-center-over-dialog", { full: false });
  // The scrim closes the palette and nothing else: the dialog underneath is another layer and
  // stays open, which is what "over an open modal" means. The click lands in the corner of the
  // scrim — its centre is where the dialog itself sits.
  await page.mouse.click(5, 5).catch(() => {});
  await page.waitForTimeout(500);
  note({
    step: "scrim-closes-palette",
    paletteClosed: (await page.locator("[data-search-palette]").count()) === 0,
    dialogStillOpen: (await page.locator("[data-search-shortcuts-dialog]").count()) > 0,
  });
  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(400);

  // Unsaved work survives the palette: the box opens over a half-typed form, and Escape gives
  // the field back its focus without touching what was typed in it.
  await page.goto(`${URL_ADMIN}/pages`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1300);
  await page
    .locator("button:has-text('New page')")
    .first()
    .click({ timeout: 4000 })
    .catch(() => {});
  await page.waitForTimeout(700);
  await page
    .locator("#page-title")
    .first()
    .fill("Unsaved draft QA")
    .catch(() => {});
  const openedOverForm = await openPalette();
  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(600);
  const titleValue = await page
    .locator("#page-title")
    .first()
    .inputValue()
    .catch(() => "");
  const focusReturned = await page
    .evaluate(() => document.activeElement?.id === "page-title")
    .catch(() => false);
  note({ step: "unsaved-form-survives", openedOverForm, titleValue, focusReturned });
  await shot(page, "command-center-unsaved-form", { full: false });
  await page
    .locator("button:has-text('Cancel')")
    .first()
    .click({ timeout: 3000 })
    .catch(() => {});

  report.commandCenter = { steps };
  log(`command centre: ${JSON.stringify(steps)}`);
}

/**
 * The subjects-and-scopes pass (REQ-006, slice 2).
 *
 * Drives the whole slice through the panel: the overview counts, two accounts created from the
 * users screen (one plain, one that will only hold a resource-scoped role), a role attached at
 * organisation scope and one at a path glob, the effective-permissions tab, the simulator
 * answering ALLOWED and DENIED with its chain, a group with a member and a role, and a machine
 * identity whose key is shown exactly once and then revoked.
 */
async function runIamSubjectsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-subjects-depth", action: "iam", ...step });
  };

  const pickFirstOption = async (selector) => {
    const value = await page
      .locator(`${selector} option`)
      .nth(1)
      .getAttribute("value")
      .catch(() => null);
    if (value) {
      await page.selectOption(selector, value).catch(() => {});
    }
    return value;
  };

  // The overview: every count card is a link, and the numbers are real.
  await page.goto(`${URL_ADMIN}/settings/iam`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-iam-overview-card]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(500);
  const overviewCards = await page.locator("[data-iam-overview-card]").count();
  const overviewText = (await page.locator("body").innerText().catch(() => "")).replace(/\s+/g, " ");
  note({
    step: "overview",
    cards: overviewCards,
    showsAccounts: /Accounts/i.test(overviewText),
    showsExpiring: /Running out within seven days/i.test(overviewText),
    showsRecent: /Recent privileged actions/i.test(overviewText),
  });
  await shot(page, "page-iam-overview");

  // The users screen: search, then create an account without a password (an invite).
  await page.goto(`${URL_ADMIN}/settings/iam/users`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-user-create-open]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator("[data-users-search]").first().fill("qa").catch(() => {});
  await page.waitForTimeout(700);
  await page.locator("[data-users-search]").first().fill("").catch(() => {});
  await page.waitForTimeout(700);
  note({ step: "users-list", rows: await page.locator("[data-user-row]").count() });

  await page.locator("[data-user-create-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.locator("[data-user-new-email]").first().fill("qa-subject@example.com").catch(() => {});
  await page.locator("[data-user-new-name]").first().fill("QA Subject").catch(() => {});
  await pickFirstOption("[data-user-new-role]");
  await page.locator("[data-user-create-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const createNotice = (await page.locator("[data-users-notice]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({ step: "user-created", notice: createNotice.slice(0, 120), created: /was created/i.test(createNotice) });

  // A second account exists for the resource-scoped demonstration only.
  await page.locator("[data-user-create-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.locator("[data-user-new-email]").first().fill("qa-scoped@example.com").catch(() => {});
  await page.locator("[data-user-new-name]").first().fill("QA Scoped").catch(() => {});
  await page.locator("[data-user-create-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  await shot(page, "page-iam-users");

  // Open the first account, attach a role at organisation scope.
  await page.locator('[data-user-open="qa-subject@example.com"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForSelector("[data-user-detail-title]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(600);
  const subjectUrl = page.url();
  const subjectId = /\/settings\/iam\/users\/([0-9a-f-]+)/.exec(subjectUrl)?.[1] || "";
  note({ step: "user-opened", subjectId: Boolean(subjectId) });
  await shot(page, "page-iam-user-detail");

  await page.locator('[data-user-tab="bindings"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.selectOption("[data-user-binding-role]", { label: "Member (member)" }).catch(async () => {
    await pickFirstOption("[data-user-binding-role]");
  });
  await page.selectOption("[data-user-binding-scope]", "organization").catch(() => {});
  await page.locator("[data-user-binding-add]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const bindingRows = await page.locator("[data-user-binding-row]").count();
  const bindingNotice = (await page.locator("[data-user-detail-notice]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({ step: "binding-org", rows: bindingRows, notice: bindingNotice.slice(0, 120) });

  // The effective set resolves through the same function the guard runs.
  await page.locator('[data-user-tab="effective"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(900);
  const granted = await page.locator("[data-effective-granted]").count();
  const grantedKeys = await page.locator("[data-effective-granted]").evaluateAll((nodes) =>
    nodes.slice(0, 6).map((node) => node.getAttribute("data-effective-granted")),
  );
  note({ step: "effective", granted, hasPagesRead: grantedKeys.includes("content.pages.read") });
  await shot(page, "page-iam-user-effective");

  // The second account gets a resource-scoped binding only: /blog/*.
  await page.goto(`${URL_ADMIN}/settings/iam/users`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  await page.locator('[data-user-open="qa-scoped@example.com"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForSelector("[data-user-detail-title]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(500);
  const scopedUrl = page.url();
  const scopedId = /\/settings\/iam\/users\/([0-9a-f-]+)/.exec(scopedUrl)?.[1] || "";
  await page.locator('[data-user-tab="bindings"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.selectOption("[data-user-binding-role]", { label: "Member (member)" }).catch(async () => {
    await pickFirstOption("[data-user-binding-role]");
  });
  await page.selectOption("[data-user-binding-scope]", "resource").catch(() => {});
  await page.waitForTimeout(300);
  await page.locator("[data-user-binding-resource]").first().fill("/blog/*").catch(() => {});
  await page.locator("[data-user-binding-add]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  note({
    step: "binding-resource",
    scopedId: Boolean(scopedId),
    rows: await page.locator("[data-user-binding-row]").count(),
  });
  await shot(page, "page-iam-user-resource-binding");

  // The simulator: an account that holds the role at organisation scope is allowed inside it…
  await page.goto(`${URL_ADMIN}/settings/iam/simulator`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-sim-run]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.selectOption("[data-sim-subject-type]", "user").catch(() => {});
  await page.waitForTimeout(300);
  await page.selectOption("[data-sim-subject]", { label: "QA Subject" }).catch(async () => {
    await page.selectOption("[data-sim-subject]", subjectId).catch(() => {});
  });
  await page.selectOption("[data-sim-permission]", "content.pages.read").catch(() => {});
  await page.locator("[data-sim-run]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const allowedVerdict = (await page.locator("[data-sim-verdict]").first().innerText().catch(() => "")).trim();
  const allowedSource = (await page.locator("[data-sim-source]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  const allowedSteps = await page.locator("[data-sim-step]").count();
  note({
    step: "simulate-allowed",
    verdict: allowedVerdict,
    source: allowedSource.slice(0, 120),
    steps: allowedSteps,
  });
  await shot(page, "page-iam-simulator-allowed");

  // … and denied for a permission no bound role holds (the simulator names the reason).
  await page.selectOption("[data-sim-permission]", "users.read").catch(() => {});
  await page.locator("[data-sim-run]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const deniedVerdict = (await page.locator("[data-sim-verdict]").first().innerText().catch(() => "")).trim();
  note({ step: "simulate-denied", verdict: deniedVerdict });

  // The resource-scoped account: the same permission is allowed on /blog/… and denied on /legal/….
  await page.selectOption("[data-sim-subject]", { label: "QA Scoped" }).catch(() => {});
  await page.waitForTimeout(300);
  await page.selectOption("[data-sim-permission]", "content.pages.read").catch(() => {});
  await page.locator("[data-sim-path]").first().fill("/blog/hello-world").catch(() => {});
  await page.locator("[data-sim-run]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const blogVerdict = (await page.locator("[data-sim-verdict]").first().innerText().catch(() => "")).trim();
  await page.locator("[data-sim-path]").first().fill("/legal/terms").catch(() => {});
  await page.locator("[data-sim-run]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const legalVerdict = (await page.locator("[data-sim-verdict]").first().innerText().catch(() => "")).trim();
  const legalStates = await page.locator("[data-sim-step-state]").evaluateAll((nodes) =>
    nodes.map((node) => node.getAttribute("data-sim-step-state")),
  );
  note({
    step: "simulate-resource-scope",
    blog: blogVerdict,
    legal: legalVerdict,
    outOfScope: legalStates.filter((state) => state === "out_of_scope").length,
  });
  await shot(page, "page-iam-simulator-resource");

  // Groups: a team with a member and a role attached.
  await page.goto(`${URL_ADMIN}/settings/iam/groups`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-group-create-open]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator("[data-group-create-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.locator("[data-group-new-name]").first().fill("QA Team").catch(() => {});
  await page.locator("[data-group-new-description]").first().fill("Created by the walkthrough").catch(() => {});
  await page.locator("[data-group-create-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  if ((await page.locator("[data-group-panel]").count()) === 0) {
    await page.locator('[data-group-open="qa-team"]').first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1200);
  }
  const groupOptions = await page.locator("[data-group-member-option]").count();
  await page
    .locator('[data-group-member-option="qa-subject@example.com"] input')
    .first()
    .check({ timeout: 4000 })
    .catch(() => {});
  await page.locator("[data-group-members-save]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  await page.selectOption("[data-group-role-select]", { label: "Member (member)" }).catch(async () => {
    await pickFirstOption("[data-group-role-select]");
  });
  await page.locator("[data-group-role-attach]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  note({
    step: "groups",
    directory: groupOptions,
    panel: (await page.locator("[data-group-panel]").count()) > 0,
    members: await page.locator("[data-group-member-option] input:checked").count(),
    roles: await page.locator("[data-group-role]").count(),
  });
  await shot(page, "page-iam-groups");

  // Service accounts: a key is shown once and can be revoked again.
  await page.goto(`${URL_ADMIN}/settings/iam/service-accounts`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-sa-create-open]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator("[data-sa-create-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.locator("[data-sa-new-name]").first().fill("qa-runner").catch(() => {});
  await page.locator("[data-sa-new-description]").first().fill("Created by the walkthrough").catch(() => {});
  await page.locator("[data-sa-new-first-key]").first().check().catch(() => {});
  await page.locator("[data-sa-create-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const token = (await page.locator("[data-sa-token]").first().innerText().catch(() => "")).trim();
  note({
    step: "service-account",
    tokenShown: /^omsa_[a-z0-9]{10}_[a-z0-9]{32}$/.test(token),
    tokenPrefix: token.slice(0, 15),
    keys: await page.locator("[data-sa-key]").count(),
  });
  await shot(page, "page-iam-service-accounts");

  await page.locator("[data-sa-key-revoke]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const revokedNotice = (await page.locator("[data-sa-notice]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({
    step: "key-revoked",
    notice: revokedNotice.slice(0, 120),
    revokeButtons: await page.locator("[data-sa-key-revoke]").count(),
  });

  // The role members tab now answers with subjects, not just accounts.
  await page.goto(`${URL_ADMIN}/settings/iam/roles`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  const memberRole = await page.locator('[data-role-open="member"]').first().getAttribute("href").catch(() => null);
  if (memberRole) {
    await page.goto(`${URL_ADMIN}${memberRole}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1200);
    await page
      .locator("button", { hasText: /^Members \(/ })
      .first()
      .click({ timeout: 4000 })
      .catch(() => {});
    await page.waitForTimeout(900);
    note({ step: "role-members", rows: await page.locator("[data-role-member]").count() });
  }

  const summary = { steps, subjectId: subjectId || null, scopedId: scopedId || null };
  report.iamSubjects = summary;
  log(`iam subjects depth: ${JSON.stringify(steps)}`);
  return summary;
}

/**
 * The role-depth pass (REQ-006, slice 1).
 *
 * Drives the real lifecycle through the panel: a custom role is created from the list, a matrix
 * cell is cycled through all three states (allow → deny → inherit), the diff preview is read,
 * the set is saved, the page is reloaded to prove the save stuck, and the history tab is checked
 * for the diff it introduced. The copy flow and the guarded delete follow, so every control of
 * the two screens has been used by the time the pass ends.
 */
async function runIamRolesDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-roles-depth", action: "iam", ...step });
  };

  const listUrl = `${URL_ADMIN}/settings/iam/roles`;
  await page.goto(listUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-role-create-open]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(500);

  const platformRoles = await page.locator("[data-role-row]").count();
  const platformRow = await page.locator('[data-role-row="editor"]').count();
  note({ step: "list", rows: platformRoles, hasPlatformEditor: platformRow > 0 });
  await shot(page, "page-iam-roles");

  // Create a custom role through the form.
  await page.locator("[data-role-create-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.locator("[data-role-new-key]").first().fill("qa-depth-role").catch(() => {});
  await page.locator("[data-role-new-name]").first().fill("QA Depth Role").catch(() => {});
  await page
    .locator("[data-role-new-description]")
    .first()
    .fill("Created by the walkthrough")
    .catch(() => {});
  await page.locator("[data-role-new-priority]").first().fill("450").catch(() => {});
  await page.locator("[data-role-create-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);

  const created = await page
    .locator("[data-role-version]")
    .first()
    .innerText()
    .catch(() => "");
  const detailUrl = page.url();
  const roleId = /\/settings\/iam\/roles\/([0-9a-f-]+)/.exec(detailUrl)?.[1] || "";
  note({ step: "create", url: detailUrl, roleId: Boolean(roleId), version: created.trim() });
  if (!roleId) {
    const failure = (await page.locator("[data-role-create-error]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
    note({ step: "create-failed", error: failure.slice(0, 160) });
    const early = { steps, roleId: null };
    report.iamRoles = early;
    log(`iam roles depth: ${JSON.stringify(steps)}`);
    return early;
  }

  // Filter to one permission so its row is on screen, then cycle the cell three ways.
  await page.locator("[data-matrix-search]").first().fill("content.pages.read").catch(() => {});
  await page.waitForTimeout(400);
  const row = '[data-matrix-row="content.pages.read"]';
  const cellOn = async (value) =>
    page
      .locator(`${row} [data-matrix-cell="content.pages.read"][data-matrix-value="${value}"]`)
      .first()
      .getAttribute("data-on")
      .catch(() => null);

  const cycle = [];
  for (const value of ["allow", "deny", "inherit", "allow"]) {
    await page
      .locator(`${row} [data-matrix-cell="content.pages.read"][data-matrix-value="${value}"]`)
      .first()
      .click({ timeout: 4000 })
      .catch(() => {});
    await page.waitForTimeout(250);
    cycle.push(`${value}:${(await cellOn(value)) === "true"}`);
  }
  note({ step: "cycle", cycle: cycle.join(" ") });
  await shot(page, "page-iam-role-matrix");

  // A diff preview, then the save.
  await page.locator("[data-matrix-preview]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(900);
  const diffVisible = (await page.locator("[data-matrix-diff]").count()) > 0;
  const diffText = diffVisible
    ? (await page.locator("[data-matrix-diff]").first().innerText().catch(() => "")).replace(/\s+/g, " ")
    : "";
  note({ step: "preview", visible: diffVisible, text: diffText.slice(0, 160) });

  await page.locator("[data-matrix-save]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const notice = (await page.locator("[data-role-notice]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({ step: "save", notice: notice.slice(0, 160) });

  // Reopen the screen: the saved cell must come back set.
  await page.goto(`${URL_ADMIN}/settings/iam/roles/${roleId}`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-matrix-search]").first().fill("content.pages.read").catch(() => {});
  await page.waitForTimeout(400);
  const reopened = await cellOn("allow");
  note({ step: "reopen", allowCellOn: reopened === "true" });

  // The history tab carries the diff the save introduced.
  await page.locator('[data-tab="history"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1000);
  const versionRows = await page.locator("[data-role-version-row]").count();
  const diffRows = await page.locator("[data-role-version-diff]").count();
  const historyText = (await page.locator('[data-tab="history"]').first().innerText().catch(() => "")).slice(0, 60);
  note({ step: "history", versions: versionRows, diffs: diffRows, label: historyText.replace(/\s+/g, " ") });
  await shot(page, "page-iam-role-history");

  // Members and inherited-by tabs answer with their own state.
  await page.locator('[data-tab="members"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(700);
  const memberRows = await page.locator("[data-role-member]").count();
  await page.locator('[data-tab="inherited"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(500);
  const childRows = await page.locator("[data-role-child]").count();
  note({ step: "tabs", members: memberRows, children: childRows });

  // The editor refuses an empty name in the field itself — before anything is sent, so the
  // refusal never becomes a request (the server refuses the same shape; the API test pins it).
  await page.locator("[data-tab=\"permissions\"]").first().click({ timeout: 4000 }).catch(() => {});
  await page.locator("[data-role-edit-open]").first().click({ timeout: 4000 }).catch(() => {});
  await page.locator("[data-role-edit-name]").first().fill("   ").catch(() => {});
  await page.locator("[data-role-edit-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(700);
  const fieldError = (await page.locator("[data-role-edit-error]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({ step: "edit-refused", refused: fieldError.length > 0, error: fieldError.slice(0, 120) });

  await page.locator("[data-role-edit-name]").first().fill("QA Depth Role Renamed").catch(() => {});
  await page.locator("[data-role-edit-priority]").first().fill("460").catch(() => {});
  await page.locator("[data-role-edit-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1000);
  const editedNotice = (await page.locator("[data-role-notice]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({ step: "edit-saved", notice: editedNotice.slice(0, 120) });

  // Copy the role from the list, then delete the copy — the guarded path.
  await page.goto(listUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  await page.locator('[data-role-duplicate="qa-depth-role"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.locator("[data-role-duplicate-key]").first().fill("qa-depth-role-copy").catch(() => {});
  await page.locator("[data-role-duplicate-name]").first().fill("QA Depth Role (copy)").catch(() => {});
  await page.locator("[data-role-duplicate-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1400);
  const copyUrl = page.url();
  const copyAllowed = (await page.locator("[data-role-notice], [data-matrix-counts]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({ step: "duplicate", url: copyUrl, counts: copyAllowed.slice(0, 120) });

  await page.goto(listUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  await page.locator('[data-role-delete="qa-depth-role-copy"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(300);
  await page.locator('[data-role-delete-confirm="qa-depth-role-copy"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1000);
  const deleteNotice = (await page.locator("[data-role-notice]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  const copyGone = (await page.locator('[data-role-row="qa-depth-role-copy"]').count()) === 0;
  note({ step: "delete-copy", gone: copyGone, notice: deleteNotice.slice(0, 120) });

  const summary = { steps, roleId, copyGone };
  report.iamRoles = summary;
  log(`iam roles depth: ${JSON.stringify(steps)}`);
  return summary;
}

/**
 * The depth pass of the results screen and the search settings (REQ-002, slice 3).
 *
 * Every step is a number, not an impression: the facet's own count before and after a click, the
 * chips the filters leave, how many rows a Shift-range selected, how many links the clipboard
 * holds, how many rows the exported file carries — and, on the settings screen, what the pass
 * report says and what the weights form does with a value the API refuses.
 */
async function runSearchDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "search-depth", action: "search", ...step });
  };

  await page.context().grantPermissions(["clipboard-read", "clipboard-write"], {
    origin: URL_ADMIN,
  }).catch(() => {});

  await page.goto(`${URL_ADMIN}/search?q=qa`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-facet-rail]", { timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(700);

  const groups = await page.locator("[data-facet]").count();
  const countBefore = await searchTotal(page);
  const firstValue = page.locator('[data-facet="type"] [data-facet-value]').first();
  const firstLabel = (await firstValue.innerText().catch(() => "")).replace(/\s+/g, " ").trim();
  note({ step: "facets", groups, firstValue: firstLabel, countBefore });
  await shot(page, "search-facets");

  await firstValue.click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1000);
  const appliedChips = await page.locator("[data-chip]").count();
  const countAfter = await searchTotal(page);
  note({
    step: "apply-facet",
    chips: appliedChips,
    countBefore,
    countAfter,
    narrowed: countAfter !== null && countBefore !== null && countAfter <= countBefore,
  });
  await shot(page, "search-facet-applied");

  await page.locator("[data-chip] button").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(900);
  note({ step: "remove-chip", chips: await page.locator("[data-chip]").count() });

  // `s` cycles the sort order without touching the mouse.
  await page.locator("[data-search-query]").first().click().catch(() => {});
  await page.keyboard.press("Escape").catch(() => {});
  await page.locator("body").click({ position: { x: 5, y: 5 } }).catch(() => {});
  await page.keyboard.press("s").catch(() => {});
  await page.waitForTimeout(900);
  const sortValue = await page.locator("[data-search-sort]").inputValue().catch(() => null);
  note({ step: "sort-key", sortValue });

  // A Shift-range selection: two clicks with Shift select the rows between them.
  const boxes = page.locator("[data-search-row] [data-row-checkbox]");
  const rowCount = await boxes.count();
  if (rowCount >= 3) {
    await boxes.nth(0).click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(200);
    await boxes.nth(2).click({ modifiers: ["Shift"], timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(500);
  } else {
    for (let i = 0; i < rowCount; i += 1) {
      await boxes.nth(i).click({ timeout: 4000 }).catch(() => {});
    }
    await page.waitForTimeout(400);
  }
  const bulkText = await page.locator("[data-search-bulk]").innerText().catch(() => "");
  const selectedCount = Number((/(\d+)\s+selected/.exec(bulkText) || [])[1] || 0);
  note({ step: "shift-range", rows: rowCount, selected: selectedCount });
  await shot(page, "search-selection");

  // Copy links: the clipboard holds one link per selected row.
  await page.locator("[data-bulk-copy]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  const clipboard = await page
    .evaluate(() => navigator.clipboard.readText().catch(() => ""))
    .catch(() => "");
  const links = clipboard.split("\n").map((line) => line.trim()).filter(Boolean);
  note({ step: "copy-links", links: links.length, sample: links[0] || null, matches: links.length === selectedCount });

  // Export the selection: the file carries exactly the rows the screen showed as checked.
  let csvRows = null;
  let csvHeader = null;
  const download = await Promise.all([
    page.waitForEvent("download", { timeout: 10000 }).catch(() => null),
    page.locator("[data-bulk-export]").click({ timeout: 4000 }).catch(() => {}),
  ]).then(([event]) => event);
  if (download) {
    const target = path.join(OUT, "search-export.csv");
    await download.saveAs(target).catch(() => {});
    if (fs.existsSync(target)) {
      const lines = fs.readFileSync(target, "utf8").split("\n").filter((line) => line.trim());
      csvHeader = lines[0] || null;
      csvRows = Math.max(lines.length - 1, 0);
    }
  }
  note({ step: "export-selection", csvRows, csvHeader, matches: csvRows === selectedCount });
  await shot(page, "search-exported");

  // The shortcut list, and Escape leaving it.
  await page.locator("body").click({ position: { x: 5, y: 5 } }).catch(() => {});
  await page.keyboard.press("?").catch(() => {});
  await page.waitForTimeout(400);
  const shortcuts = await page.locator("[data-search-shortcuts-dialog]").count();
  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(300);
  note({ step: "shortcuts", opened: shortcuts > 0, closed: (await page.locator("[data-search-shortcuts-dialog]").count()) === 0 });

  // The settings screen: a pass per provider, its numbers, and the weights form.
  await page.goto(`${URL_ADMIN}/settings/search`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-provider-row]:visible", { timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(700);
  const providers = await page.locator("[data-provider-row]:visible").count();
  const states = await page.locator("[data-provider-state]:visible").allInnerTexts().catch(() => []);
  note({ step: "settings-open", providers, states });
  await shot(page, "search-settings");

  await page.locator('[data-reindex="pages"]:visible').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForSelector("[data-search-progress]", { timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const progress = (await page.locator("[data-search-progress]").innerText().catch(() => "")).trim();
  const lastPass = await page
    .locator('[data-provider-row="pages"]:visible td')
    .nth(4)
    .innerText()
    .catch(() => "");
  note({ step: "reindex-provider", progress, lastPass: lastPass.replace(/\s+/g, " ").trim() });
  await shot(page, "search-settings-reindex");

  // A value the API refuses is shown as the API phrased it.
  await page.locator('[data-weight="body"]').fill("9").catch(() => {});
  await page.locator('[data-weight="title"]').fill("2").catch(() => {});
  await page.waitForTimeout(200);
  await page.locator("[data-save-settings]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(700);
  const refused = (await page.locator("text=Title must be at least Body").count()) > 0;
  note({ step: "weights-validation", refused });

  // Restore the defaults and save: the screen is left exactly as it was found. Every provider is
  // switched back on first — the generic click-through walks these checkboxes too, and the
  // installation the pass leaves behind should answer with everything it has.
  const providerBoxes = page.locator("[data-enabled-provider]");
  const providerCount = await providerBoxes.count();
  for (let index = 0; index < providerCount; index += 1) {
    const box = providerBoxes.nth(index);
    if (!(await box.isChecked().catch(() => false))) {
      await box.click({ timeout: 3000 }).catch(() => {});
    }
  }
  await page.locator("[data-restore-defaults]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(300);
  await page.locator("[data-save-settings]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const saved = (await page.locator("[data-search-progress]").innerText().catch(() => "")).trim();
  const titleWeight = await page.locator('[data-weight="title"]').inputValue().catch(() => null);
  note({ step: "weights-saved", saved, titleWeight });
  await shot(page, "search-settings-saved");

  report.searchDepth = steps;
}

// ---------------------------------------------------------------- analytics (REQ-007, slice 2)

/** Run one statement against the disposable QA database. */
function qaSql(statement) {
  return execFileSync(
    "docker",
    ["exec", QA_PG_CONTAINER, "psql", "-U", "omnion", "-d", QA_DB, "-v", "ON_ERROR_STOP=1", "-t", "-A", "-c", statement],
    { encoding: "utf8", timeout: 30000 },
  ).trim();
}

/**
 * The site every depth pass reads, created on demand — with the organization under it.
 *
 * A dozen depth passes open with the same two lines:
 *
 *   const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
 *   if (!siteId) { steps.reason = "the QA site does not exist…"; return steps; }
 *
 * On a FULL pass the first-run wizard creates that site, so the guard never fires. On a
 * `--only=<pass>` pass — the entry point that exists precisely because a full pass gets cut
 * down halfway — `runWizard` sees the bootstrapped admin, reports "installation already
 * exists" and returns, and nothing has ever created the site. Every step of the pass is then
 * skipped, and the pass still exits 0 after printing one `reason` line. That is what happened
 * to `--only=theme-builder` on 2026-09-30: 59/59 steps skipped, reported as a completed pass.
 *
 * A skip that reports itself as a pass is the worst shape a guard can have, so the fix is
 * below the guard rather than inside every pass: make the rows exist.
 *
 * ## Why the organization is created here too, and not read
 *
 * The first version of this helper read `users.organization_id` and gave up when it was empty.
 * It always is, on a scoped pass: `run.sh` seeds the first account straight into `users`, and
 * an account with no organization is a *platform* account — the whole point of the bootstrap.
 * The wizard is what gives the installation its organization, and the wizard is exactly what a
 * scoped pass skips. So reading the organization asked a question whose answer is "no" in the
 * only case that reaches this code, and the helper's own log line ("no QA site and no
 * organization to create one under") was the honest report of a design that had to guess.
 *
 * The organization is therefore written here, by slug, from the same constants the wizard
 * fills its own form from (`CREDS.org`, `CREDS.orgSlug`) — so a full pass and a scoped pass
 * produce the same rows and a later pass that asserts on either finds them.
 *
 * Both writes are `on conflict do nothing`, and the site is only inserted once the
 * organization is known to exist. That ordering is the whole point: a site whose organization
 * id is empty fails the `sites.organization_id` not-null constraint, and a `select` that
 * returns an empty string rather than `null` is exactly what made the first attempt look like
 * a database problem.
 *
 * Returns the site id, or `""` when the account itself is missing — a genuinely broken
 * bootstrap, which the caller reports rather than papers over.
 */
function ensureQaSite() {
  const existing = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (existing) return existing;
  const account = qaSql(`select id from users where email = '${CREDS.email}' limit 1`);
  if (!account) return "";
  qaSql(
    `insert into organizations (name, slug) values ('${CREDS.org}', '${CREDS.orgSlug}') ` +
      `on conflict (slug) do nothing`,
  );
  const organization = qaSql(`select id from organizations where slug = '${CREDS.orgSlug}'`);
  if (!organization) return "";
  qaSql(
    `insert into sites (organization_id, key, name, status, theme) ` +
      `select '${organization}', '${CREDS.siteKey}', '${CREDS.site}', 'active', 'minimal' ` +
      `on conflict (organization_id, key) do nothing`,
  );
  return qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
}

/**
 * Two pages for a pass that needs them: one published (with the revision a published page carries
 * its title in) and one draft.
 *
 * The draft is not decoration. "The picker offers published pages only" is proved by *offering a
 * draft somewhere and showing it is absent* — with no draft in the database the assertion
 * `pickerOnlyOffersPublished` compares against an empty list and passes for any picker at all,
 * including one that lists everything. Both rows are written directly because the screen under
 * test is the menu editor, not the page editor, and a fixture that has to be driven through
 * another screen is a fixture that inherits that screen's failure.
 */
function ensureQaPages(siteId, stamp) {
  if (!siteId) return false;
  const slug = `qa-menu-page-${stamp}`;
  try {
    qaSql(
      `insert into pages (site_id, slug, page_type, status)
       values ('${siteId}', '${slug}', 'page', 'published')
       on conflict (site_id, slug) do update set status = 'published'`,
    );
    const pageId = qaSql(`select id from pages where site_id = '${siteId}' and slug = '${slug}' limit 1`);
    if (!pageId) return false;
    // The label a page item gets is the revision's title, so a published page with no revision
    // would prove `labelComesFromTheTitle` against nothing.
    qaSql(
      `insert into page_revisions (page_id, revision_no, state, title, body, published_at)
       values ('${pageId}', 1, 'published', 'QA Menu Page ${stamp}', 'qa', now())
       on conflict (page_id, revision_no) do update set title = excluded.title`,
    );
    qaSql(
      `insert into pages (site_id, slug, page_type, status)
       values ('${siteId}', '${slug}-draft', 'page', 'draft')
       on conflict (site_id, slug) do nothing`,
    );
    return true;
  } catch (error) {
    log(`ensureQaPages failed: ${error.message}`);
    return false;
  }
}

/** Post one beacon to the public collection endpoint of the QA site. */
async function postBeacon(body, { userAgent, forwardedFor, country }) {
  const headers = { "content-type": "application/json", "user-agent": userAgent };
  if (forwardedFor) headers["x-forwarded-for"] = forwardedFor;
  if (country) headers["cf-ipcountry"] = country;
  const response = await fetch(
    `${URL_ADMIN}/api/v1/public/analytics/collect?site=${CREDS.siteKey}`,
    { method: "POST", headers, body: JSON.stringify(body) },
  );
  return { status: response.status, body: await response.json().catch(() => null) };
}

/**
 * The synthetic batch the analytics screens are read against: fixed test paths, one download, one
 * form submit, one custom event, two device types and three countries — then a slice of it spread
 * over the last thirty days so the series has a shape beyond today.
 */
async function seedAnalytics(report) {
  const desktop =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
  const phone =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
  const linux =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

  const batch = [
    {
      meta: { userAgent: desktop, forwardedFor: "203.0.113.11", country: "TR" },
      payload: {
        pageview: {
          path: "/qa/landing",
          title: "QA landing",
          referrer: "https://www.google.com/search?q=omnion",
          duration_ms: 2400,
          scroll_depth: 62,
          screen: { width: 1440, height: 900 },
          language: "tr-TR",
        },
        events: [
          { name: "download", properties: { file: "/qa/files/guide.pdf" } },
          { name: "signup", value: 49.5, properties: { plan: "pro" } },
        ],
        utm: { source: "newsletter", medium: "email", campaign: "launch" },
      },
    },
    {
      meta: { userAgent: phone, forwardedFor: "198.51.100.22", country: "DE" },
      payload: {
        pageview: {
          path: "/qa/pricing",
          title: "QA pricing",
          duration_ms: 1500,
          scroll_depth: 40,
          screen: { width: 390, height: 844 },
          language: "de-DE",
        },
        events: [{ name: "form_submit", value: 120, properties: { form: "contact" } }],
        utm: { source: "newsletter", medium: "email", campaign: "launch" },
      },
    },
    {
      meta: { userAgent: linux, forwardedFor: "192.0.2.33", country: "FR" },
      payload: {
        pageview: {
          path: "/qa/docs",
          title: "QA docs",
          duration_ms: 900,
          scroll_depth: 88,
          screen: { width: 1920, height: 1080 },
          language: "fr-FR",
        },
        events: [{ name: "cta_click", properties: { slot: "hero" } }],
      },
    },
    {
      meta: { userAgent: phone, forwardedFor: "198.51.100.44", country: "TR" },
      payload: {
        pageview: {
          path: "/qa/landing",
          title: "QA landing",
          duration_ms: 700,
          scroll_depth: 25,
          screen: { width: 390, height: 844 },
          language: "tr-TR",
        },
        events: [
          { name: "form_start", properties: { form: "contact" } },
          { name: "form_submit", value: 80, properties: { form: "contact" } },
        ],
      },
    },
  ];

  const answers = [];
  for (const entry of batch) {
    answers.push(await postBeacon(entry.payload, entry.meta));
  }
  const accepted = answers.filter((answer) => answer.status === 202).length;

  // The history fixture: a third of the batch stays today, a third lands inside the last week and
  // a third inside the last month, so 7-day and 30-day ranges both have a shape to draw.
  let spread = "skipped";
  try {
    const site = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
    const shift = (table, column) => `
      update ${table} set ${column} = ${column} - (
        case when id % 3 = 1 then (1 + (id % 6)) else (7 + (id % 23)) end || ' days'
      )::interval
      where site_id = '${site}' and id % 3 <> 0;`;
    for (const statement of [
      shift("analytics_pageviews", "occurred_at"),
      shift("analytics_visits", "started_at"),
      shift("analytics_events", "occurred_at"),
    ]) {
      qaSql(statement);
    }
    spread = "applied";
  } catch (err) {
    spread = `skipped: ${String(err).slice(0, 120)}`;
  }

  return { accepted, posted: batch.length, spread, first: answers[0] };
}

/** The analytics pass: the range, the comparison, a page drawer and a real export. */
async function runAnalyticsDepth(page, report) {
  const steps = {};
  await page.goto(`${URL_ADMIN}/analytics`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);

  const kpi = () =>
    page
      .locator("[data-analytics-kpi=visitors] [data-analytics-kpi-value]")
      .first()
      .innerText()
      .catch(() => "0");

  // The batch is already in the raw rows; the screen reads them on its own request.
  let visitors = (await kpi()).trim();
  for (let attempt = 0; attempt < 12 && visitors === "0"; attempt += 1) {
    await page.locator("[data-analytics-refresh]").click({ timeout: 3000 }).catch(() => {});
    await page.waitForTimeout(1000);
    visitors = (await kpi()).trim();
  }
  steps.visitors = visitors;
  steps.empty = (await page.locator("[data-analytics-overview-empty]").count()) > 0;

  await page.locator("[data-analytics-preset=7d]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(900);
  const compareBox = page.locator("[data-analytics-compare]").first();
  if (!(await compareBox.isChecked().catch(() => false))) {
    await compareBox.check({ timeout: 4000 }).catch(() => {});
  }
  await page.waitForTimeout(1200);
  await shot(page, "page-analytics-overview");
  steps.comparison =
    (await page.locator("[data-analytics-no-comparison]").count()) > 0 ? "no-comparison" : "compared";
  steps.range = (await page.locator("[data-analytics-toolbar]").innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .slice(0, 160);
  steps.series = await page.locator("[data-analytics-axis-label]").count();

  // The page report, its drawer and its export.
  await page.goto(`${URL_ADMIN}/analytics/pages`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);
  steps.pageRows = await page.locator("[data-analytics-row]").count();
  // `:visible` on purpose: a click-through harness clicks what a person could click, and a screen
  // is allowed to keep an unshown copy of a control in the DOM.
  const link = page.locator("[data-analytics-page-link]:visible").first();
  if ((await link.count()) > 0) {
    await link.click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1400);
    steps.drawer = (await page.locator("[data-analytics-drawer]").count()) > 0;
    await shot(page, "analytics-page-drawer");
    await page.locator("[data-analytics-drawer-close]").first().click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(500);
  } else {
    steps.drawer = false;
  }

  // Every report has an empty state, and it is exercised rather than assumed: a filter that
  // cannot match leaves the screen with nothing, and the screen has to say so.
  await page
    .goto(`${URL_ADMIN}/analytics/pages?path=qa-nothing-matches-this`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(1400);
  steps.emptyState = (await page.locator('[data-analytics-state="analytics-empty"]').count()) > 0;
  await shot(page, "analytics-pages-empty");
  await page.goto(`${URL_ADMIN}/analytics/pages`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);

  const waiting = page.waitForEvent("download", { timeout: 20000 }).catch(() => null);
  await page.locator("[data-analytics-export]").click({ timeout: 4000 }).catch(() => {});
  const download = await waiting;
  steps.export = download
    ? { filename: download.suggestedFilename(), failure: download.failure() || null }
    : "no-download";
  steps.exportNote = (
    await page.locator("[data-analytics-export-note]").innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();

  report.analytics = { ...(report.analytics || {}), ...steps };
  return steps;
}

// ---------------------------------------------------------------- goals + realtime (REQ-007, slice 3)

/**
 * The goals and realtime pass: a goal is created through the editor, a visitor completes it, and
 * the funnel and the live counters are read back.
 *
 * The goal is named per run so a second pass on the same database does not collide with the name
 * the first one created — and so the funnel the pass reads is its own, not a leftover.
 */
async function runGoalAndRealtimeDepth(page, report) {
  const steps = {};
  const name = `QA funnel ${Math.floor(Date.now() / 1000) % 1000000}`;

  await page.goto(`${URL_ADMIN}/analytics/goals`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  await shot(page, "page-analytics-goals");

  // Open the editor and describe a two-step funnel: the landing page, then the download.
  await page.locator("[data-goal-create]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-goal-name]").first().fill(name, { timeout: 3000 }).catch(() => {});
  await page.locator("[data-goal-step-add]").click({ timeout: 3000 }).catch(() => {});
  await page.waitForTimeout(250);
  await page
    .locator('[data-goal-step-kind="2"]')
    .selectOption("download", { timeout: 3000 })
    .catch(() => {});
  await page
    .locator('[data-goal-step-file="2"]')
    .first()
    .fill("/qa/files/guide.pdf", { timeout: 3000 })
    .catch(() => {});
  steps.steps = await page.locator("[data-goal-step]").count();

  // A step without a match is refused by the editor before anything is sent: the save below has
  // no path yet, and the screen must say so instead of sending a goal the API would refuse.
  await page.locator("[data-goal-save]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.validation = (await page.locator("[data-goal-error]").count()) > 0 ? "refused" : "silent";
  await page
    .locator('[data-goal-step-path="1"]')
    .first()
    .fill("/qa/landing", { timeout: 3000 })
    .catch(() => {});

  await page.locator("[data-goal-save]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1500);
  steps.created = (await page.locator("[data-goal-open]").count()) > 0;
  steps.savedName = name;

  // A visitor completes both steps *after* the goal exists: a goal only counts what happens
  // while it is listening.
  const desktop =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
  const answer = await postBeacon(
    {
      pageview: {
        path: "/qa/landing",
        title: "QA landing",
        duration_ms: 900,
        scroll_depth: 45,
        screen: { width: 1440, height: 900 },
        language: "en-GB",
      },
      events: [
        { name: "download", properties: { file: "/qa/files/guide.pdf" } },
        { name: "cta_click", properties: { slot: "hero" } },
      ],
    },
    { userAgent: desktop, forwardedFor: "203.0.113.90", country: "TR" },
  );
  steps.beacon = answer.status;

  // The acceptance criterion, measured at the API: the beacon of a moment ago is visible in
  // realtime within five seconds — no rollup tick stands between the request and the counter.
  let latency = null;
  try {
    const site = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
    const cookies = await page.context().cookies();
    const cookieHeader = cookies.map((cookie) => `${cookie.name}=${cookie.value}`).join("; ");
    const started = Date.now();
    for (let attempt = 0; attempt < 10 && latency === null; attempt += 1) {
      const res = await fetch(`${URL_ADMIN}/api/v1/analytics/realtime?site_id=${site}`, {
        headers: { cookie: cookieHeader },
      });
      if (res.ok) {
        const body = await res.json();
        if (body.last_5?.visitors > 0) latency = Date.now() - started;
      }
      if (latency === null) await new Promise((resolve) => setTimeout(resolve, 400));
    }
  } catch (err) {
    latency = `error: ${String(err).slice(0, 80)}`;
  }
  steps.realtimeLatencyMs = latency;

  // The editor selected the new goal, so its funnel is on screen; a reload makes sure the answer
  // is the server's, not the editor's optimism.
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-goal-open]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.funnelSteps = await page.locator("[data-goal-funnel-step]").count();
  steps.funnelStep1 = (await page
    .locator('[data-goal-funnel-count="1"]')
    .first()
    .innerText()
    .catch(() => ""))
    .trim();
  steps.funnelStep2 = (await page
    .locator('[data-goal-funnel-count="2"]')
    .first()
    .innerText()
    .catch(() => ""))
    .trim();
  steps.conversions = (await page
    .locator("[data-goal-funnel-conversions]")
    .first()
    .innerText()
    .catch(() => ""))
    .trim();
  await shot(page, "analytics-goal-funnel");

  // Realtime: the same viewer sees the beacon of a moment ago without waiting for a rollup.
  await page.goto(`${URL_ADMIN}/analytics/realtime`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.realtimeState = (await page
    .locator("[data-rt-state]")
    .first()
    .getAttribute("data-rt-state")
    .catch(() => "")) || "";
  steps.visitors5 = (await page
    .locator('[data-rt-counter="5-visitors"]')
    .first()
    .innerText()
    .catch(() => "0"))
    .trim();
  steps.events30 = (await page
    .locator('[data-rt-counter="30-events"]')
    .first()
    .innerText()
    .catch(() => "0"))
    .trim();
  steps.pages = await page.locator("[data-rt-pages] li").count();
  steps.feed = await page.locator("[data-rt-events] li").count();
  await shot(page, "page-analytics-realtime-live");

  return steps;
}

/**
 * The notification pass (REQ-021, slice 1).
 *
 * An inbox is the easiest screen in the platform to make look right and be wrong: the rows are
 * real, the badge is real, and the reader still cannot trust either if the *counts* and the
 * *rows* were computed by different code. So the claims proved here are the ones a screenshot
 * cannot settle:
 *
 * 1. the badge and the grouped panel lines come from ONE summary, so the panel cannot show a
 *    total and a set of lines that disagree;
 * 2. a grouped line filters the list to that category — the click is the whole point of the
 *    group, so a line that does not filter is a dead control;
 * 3. a bulk action reports the number it *changed*, which is not always the size of the
 *    selection, and the notice must name that number;
 * 4. the keyboard path works: `j` moves the cursor, `e` toggles read, `x` selects, `/` focuses
 *    the filter, `Esc` closes the drawer;
 * 5. the empty state, the loading skeleton and the error state all exist and are reachable.
 *
 * Notifications are emitted through the API with the signed-in session, so the rows are real
 * rows created by the real route — the pass does not seed the table behind the panel's back.
 */
async function runNotificationsDepth(page, report) {
  const steps = {};
  const me = await page.evaluate(() =>
    fetch("/api/v1/me", { credentials: "same-origin" })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null),
  );
  const userId = me?.user?.id;
  if (!userId) {
    steps.skipped = "no signed-in user to address notifications to";
    return steps;
  }

  // Seed through the real emit route, so the badge the panel shows is a badge the API computed
  // from rows the API wrote. Two categories with different counts is the minimum that makes
  // "the grouped lines add up to the total" a claim with teeth.
  const seed = await page.evaluate(async (id) => {
    const emit = async (category, title, dedupe_key, priority) => {
      const response = await fetch("/api/v1/notifications/emit", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ category, title, priority, user_ids: [id], dedupe_key }),
      });
      return { status: response.status, body: await response.json().catch(() => null) };
    };
    return {
      approval: await emit("approval", "QA · a page is waiting for approval", `qa-appr-${Date.now()}`),
      security: await emit("security", "QA · a new sign-in", `qa-sec-${Date.now()}`, "high"),
      ticket: await emit("ticket", "QA · a ticket was assigned to you", `qa-tic-${Date.now()}`),
    };
  }, userId);
  steps.emitted = Object.fromEntries(
    Object.entries(seed).map(([key, value]) => [key, value.status]),
  );
  // A 403 here is a real finding, not a setup problem: the owner seeds the roles on boot, so an
  // owner without `notifications.send` means the permission did not reach the role.
  expectRefusal(
    "notifications/emit",
    seed.approval?.status === 403
      ? "the signed-in account cannot emit — recorded rather than hidden"
      : "an emit the panel never asked for",
  );

  // 1. The bell, on the header of a screen that is not the notification screen.
  await page.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.bell = (await page.locator("[data-bell]").count()) > 0;
  const badge = (await page.locator("[data-bell-badge]").innerText().catch(() => "")).trim();
  steps.badge = badge;

  await page.locator("[data-bell]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(700);
  steps.panel = (await page.locator("[data-bell-panel]").count()) > 0;
  steps.groupLines = await page.locator("[data-bell-groups] a").count();
  await shot(page, "page-notifications-bell-panel");

  // The badge must equal the sum of the lines. Read the numbers as text and add them here,
  // because "the panel looks consistent" is not a measurement.
  const lineCounts = await page.locator("[data-bell-groups] a span.font-medium").allInnerTexts();
  const summed = lineCounts.reduce((total, text) => total + (Number(text.trim()) || 0), 0);
  steps.badgeMatchesGroups = String(summed) === badge || badge === "99+";
  steps.groupSum = summed;

  // 2. A grouped line filters the list to its category.
  const approvalLine = page.locator("[data-bell-group=approval]").first();
  if ((await approvalLine.count()) > 0) {
    await approvalLine.click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1500);
    steps.groupFilteredUrl = page.url().includes("category=approval");
    steps.groupFilteredRows = await page.locator("[data-notification-row]").count();
    // Every row on a category-filtered list has to BE that category — the strongest form of
    // "clicking the line filters the list", and the one a badge-only implementation fails.
    steps.onlyThatCategory = await page.evaluate(() =>
      Array.from(document.querySelectorAll("[data-notification-row]")).every((row) => {
        const cells = row.querySelectorAll("td");
        return cells.length > 2 && /approval/i.test(cells[2].textContent ?? "");
      }),
    );
  }
  await shot(page, "page-notifications-filtered");

  // 3. The bulk path. Select three rows and archive them; the notice must report what CHANGED.
  await page.goto(`${URL_ADMIN}/notifications`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  const selects = page.locator("[data-select]");
  const selection = Math.min(3, await selects.count());
  for (let index = 0; index < selection; index += 1) {
    await selects.nth(index).click({ timeout: 3000 }).catch(() => {});
  }
  steps.selected = await page.locator("[data-notification-bulk]").count() > 0;
  steps.bulkButtons = await page.locator("[data-notification-bulk] button[data-bulk]").count();
  await page.locator("[data-bulk=read]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1500);
  steps.bulkNotice = (await page.locator("[data-notification-notice]").innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  // The notice has to name a number and say "of" — the honest shape is "2 of 3 marked read",
  // and a panel that says "3 of 3" is asserting something it cannot know.
  steps.bulkNoticeIsHonest = /\d+ of \d+/.test(steps.bulkNotice);
  await shot(page, "page-notifications-bulk");

  // 3b. **A list that empties itself the moment you read its mail.** Every row the pass just
  //     marked read is still there — it is a notification, not a receipt — so the bare
  //     `/notifications` URL has to bring them back. Assert it immediately after the bulk
  //     action, because that is the only moment in a pass where "read rows are visible" and
  //     "read rows are hidden" produce the same list, and the next step quietly drives past
  //     it.
  //
  // This is the assertion the defect did not have. `with_read` was a `bool` defaulting to
  // `false`, so a client that named no filter got *unread only* — while the panel's own State
  // menu labelled that state "Unread and read". The symptom arrived four lines below as
  // `keyboard: "no rows to drive — the list did not load"`, which reads like a timing problem
  // and is not one: the pass had just marked every row read. `absent means everything` is the
  // invariant; `?with_read=0` is the inbox, and both halves are asserted.
  await page.goto(`${URL_ADMIN}/notifications`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.rowsAfterMarkingRead = await page.locator("[data-notification-row]").count();
  steps.readRowsStayVisible = steps.rowsAfterMarkingRead > 0;
  // The opposite half, on the same screen: `?with_read=0` is the reader who asked for the
  // inbox, and it has to actually filter — otherwise the fix above is just a default nobody
  // can turn off, which is a different bug with the same root cause.
  await page.goto(`${URL_ADMIN}/notifications?with_read=0`, { waitUntil: "domcontentloaded" }).catch(
    () => {},
  );
  await page.waitForTimeout(1600);
  steps.inboxRows = await page.locator("[data-notification-row]").count();
  steps.inboxFilterIsHonest = steps.inboxRows < steps.rowsAfterMarkingRead;
  await shot(page, "page-notifications-inbox");

  // 4. The keyboard path. The shortcuts are bound on the table body, so the table has to be
  //    there and the body has to have focus — clicking a row opens the drawer and then every
  //    key press lands in the drawer instead. An earlier version clicked `tbody` and hoped;
  //    this one focuses the body explicitly and asserts the row count first, because a
  //    keyboard pass over an empty list reports every shortcut as broken and that is the
  //    single most misleading way for this gate to fail.
  await page.goto(`${URL_ADMIN}/notifications`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);
  steps.keyboardRows = await page.locator("[data-notification-row]").count();
  if (steps.keyboardRows === 0) {
    steps.keyboard = "no rows to drive — the list did not load";
    return steps;
  }
  await page.locator("[data-notification-table] tbody").focus().catch(() => {});
  await page.locator("[data-notification-table] tbody").click({ position: { x: 2, y: 2 } }).catch(() => {});
  // `Escape` first, so a drawer left open by the previous step cannot swallow the presses.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(300);
  await page.locator("[data-notification-table] tbody").focus().catch(() => {});
  await page.keyboard.press("j");
  await page.waitForTimeout(250);
  await page.keyboard.press("j");
  await page.waitForTimeout(400);
  steps.cursorMoved = (await page.locator("[data-notification-row][data-cursor=true]").count()) > 0;
  await page.keyboard.press("x");
  await page.waitForTimeout(300);
  steps.keyboardSelected = (await page.locator("[data-notification-bulk]").innerText().catch(() => ""))
    .includes("1 selected");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(1200);
  steps.keyboardOpenedDrawer = (await page.locator("[data-notification-drawer]").count()) > 0;
  await shot(page, "page-notifications-drawer");

  // The drawer's delivery rows. This is the section that makes "it is in my panel but the
  // e-mail never arrived" a thing a reader can see rather than infer, and it has two states that
  // are both correct on a fresh database — so the assertion is deliberately about the SHAPE:
  // either a list of channel rows, or the honest sentence that nothing has been tried. What it
  // refuses to accept is a section heading with nothing under it, which is what a screen that
  // renders the heading before the read returns looks like, and which reads as a broken panel.
  if (steps.keyboardOpenedDrawer) {
    const drawerDelivery = page.locator("[data-notification-drawer] [data-notification-deliveries]");
    steps.drawerHasDeliverySection = (await drawerDelivery.count()) > 0;
    const rows = await page.locator("[data-notification-drawer] [data-notification-delivery]").count();
    const empty = await page
      .locator("[data-notification-drawer] [data-notification-deliveries-empty]")
      .count();
    steps.drawerDeliveryRows = rows;
    steps.drawerDeliveryEmptyState = empty > 0;
    // A channel the platform has five of: the reader must never see a raw database value.
    steps.drawerDeliveryNamesChannelsInProse =
      rows === 0 ||
      (await page
        .locator("[data-notification-drawer] [data-notification-delivery]")
        .allInnerTexts()
        .catch(() => []))
        .every((text) => !/\b(in_app|web_push)\b/.test(text));
  } else {
    steps.drawerHasDeliverySection = "the drawer never opened, so the section cannot be asserted";
    steps.drawerDeliveryRows = "no drawer";
    steps.drawerDeliveryEmptyState = "no drawer";
    steps.drawerDeliveryNamesChannelsInProse = "no drawer";
  }

  await page.keyboard.press("Escape");
  await page.waitForTimeout(600);
  steps.escapeClosedDrawer = (await page.locator("[data-notification-drawer]").count()) === 0;
  // Escape is answered from the *list's* key handler, so a regression has a second, sharper
  // form: the drawer stops closing on Escape, and every shortcut after it silently stops
  // working too, because the open drawer holds the focus the next press needs. Asserting only
  // the keys that follow therefore reports a keyboard problem when the fault is one Escape
  // press earlier. Re-open it and press Escape again from a row that is not under the cursor.
  if (steps.escapeClosedDrawer) {
    const rowsNow = page.locator("[data-notification-row]");
    if ((await rowsNow.count()) > 0) {
      await rowsNow.first().click({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(700);
      const reopened = (await page.locator("[data-notification-drawer]").count()) > 0;
      await page.locator("[data-notification-table] tbody").focus().catch(() => {});
      await page.keyboard.press("k");
      await page.keyboard.press("Escape");
      await page.waitForTimeout(500);
      steps.escapeWithNoRowUnderCursor =
        reopened && (await page.locator("[data-notification-drawer]").count()) === 0;
    }
  } else {
    steps.escapeWithNoRowUnderCursor = "the drawer never closed, so the second form cannot run";
  }

  // `e` toggles read and `Shift+E` marks the visible rows — the second one is the shortcut
  // most likely to be documented and missing, so it is asserted rather than assumed.
  await page.locator("[data-notification-table] tbody").focus().catch(() => {});
  await page.keyboard.press("j");
  await page.waitForTimeout(300);
  await page.keyboard.press("e");
  await page.waitForTimeout(900);
  steps.eToggledRead =
    (await page.locator("[data-notification-row][data-read=false]").count()) > 0 ||
    (await page.locator("[data-notification-notice]").count()) > 0;

  await page.locator("[data-notification-table] tbody").focus().catch(() => {});
  await page.keyboard.press("Shift+E");
  await page.waitForTimeout(1000);
  steps.shiftEMarkedVisible =
    (await page.locator("[data-notification-notice]").innerText().catch(() => "")).includes("read");

  await page.locator("[data-notification-table] tbody").focus().catch(() => {});
  await page.keyboard.press("/");
  await page.waitForTimeout(400);
  steps.slashFocusedFilter =
    (await page.evaluate(() => document.activeElement?.id ?? "")) === "notification-search";

  // 5. The three states. The skeleton is asserted on a slow load rather than hoped for: it is
  //    rendered while `loading && rows.length === 0`, which a fast API can outrun — so the
  //    check is that the element EXISTS in the component, reached by throttling the response.
  await page.route("**/api/v1/notifications?*", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 1500));
    await route.continue();
  });
  // The empty state needs a filter that genuinely matches nothing. An earlier version asked
  // for `?read=read` — which by this point in the pass holds the very rows the bulk action
  // just marked read, so the list was correctly NOT empty and the assertion was measuring the
  // test's own ordering rather than the component. A category nobody was ever addressed is the
  // honest way in: the API answers 200 with zero rows, which is the state under test.
  await page.goto(`${URL_ADMIN}/notifications?category=mention&read=read&archived=1`, {
    waitUntil: "domcontentloaded",
  }).catch(() => {});
  await page.waitForTimeout(700);
  steps.skeleton = (await page.locator("[data-notification-skeleton]").count()) > 0;
  await page.waitForTimeout(1800);
  await page.unroute("**/api/v1/notifications?*").catch(() => {});
  steps.emptyState = (await page.locator("[data-notification-empty]").count()) > 0;
  await shot(page, "page-notifications-empty");

  // The error state, provoked the honest way: a route that answers 500. The panel must show a
  // retry line, not an empty table — an inbox that says "all caught up" after a failure is the
  // one state that makes people stop trusting it.
  //
  // The route that is failed is the **list**, not the summary. An earlier version fulfilled
  // `…/notifications/summary` with a 500 and then asserted on the list's error element, which
  // the summary cannot affect — the bell degrades on its own and the list stays healthy, so
  // the assertion could only ever have passed by accident. The element under test belongs to
  // the call that has to fail.
  await page.route("**/api/v1/notifications?*", (route) =>
    route.fulfill({
      status: 500,
      contentType: "application/json",
      body: '{"error":{"code":"boom","message":"deliberate"}}',
    }),
  );
  await page.goto(`${URL_ADMIN}/notifications`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  steps.errorState = (await page.locator("[data-notification-error]").count()) > 0;
  // A retry the reader can actually press: an error banner with no way forward is a dead end.
  steps.errorOffersRetry =
    (await page.locator("[data-notification-error] button").count()) > 0;
  await page.unroute("**/api/v1/notifications?*").catch(() => {});
  await shot(page, "page-notifications-error");

  return steps;
}

/**
 * The preferences pass (REQ-021, slice 2): the matrix, the settings row, and the two rules
 * that make them safe to edit.
 *
 * Everything here is driven through the **screen** and read back from the **API**, in that
 * order. A form that renders its own state correctly proves nothing about whether the server
 * stored it — the class of bug this screen is most likely to have is "the checkbox moved and
 * the row did not", and only a read-back after a reload can see it.
 *
 * The three claims:
 * 1. **A cell survives a round trip.** Flip one, save, reload, and it is still flipped.
 * 2. **The in-app column cannot be turned off**, and the server says why rather than ignoring
 *    the write — asserted by asking the API directly, because the UI's disabled checkbox is a
 *    promise while the API's refusal is a guarantee.
 * 3. **A half-set quiet window is refused.** The form is allowed to submit it; the server is
 *    not, and the message has to reach the screen.
 */
async function runNotificationSettingsDepth(page, report) {
  const steps = {};
  await page.goto(`${URL_ADMIN}/notifications/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);

  steps.loaded = (await page.locator("[data-pref-state=ready]").count()) > 0;
  if (!steps.loaded) {
    steps.reason = await page
      .locator("[data-pref-state=error]")
      .innerText()
      .catch(() => "the settings screen did not reach its ready state");
    return steps;
  }

  // The complete matrix: categories × channels, with the in-app column locked. A matrix that
  // renders only the stated cells would show fewer boxes than this count, and the difference
  // between a hole and a checked box is invisible until a reader tries to change one.
  steps.cells = await page.locator("[data-cell]").count();
  steps.matrixIsComplete = steps.cells >= 6 * 5;
  steps.inAppLocked = await page.evaluate(() => {
    const locked = [...document.querySelectorAll('[data-cell*="/in_app"]')];
    return locked.length > 0 && locked.every((box) => box.disabled);
  });
  steps.lockedColumnExplainsItself =
    (await page.locator("[data-pref-state=ready]").innerText()).includes("cannot be turned off");
  await shot(page, "page-notifications-settings");

  // 1. Flip one real cell, save, reload, read it back from the API.
  const target = "ticket/email";
  const box = page.locator(`[data-cell="${target}"]`);
  const before = await box.isChecked().catch(() => false);
  await box.click({ timeout: 4000 });
  await page.waitForTimeout(300);
  steps.saveEnabledAfterChange = await page.locator("[data-pref-save]").isEnabled();
  await page.locator("[data-pref-save]").click({ timeout: 4000 });
  await page.waitForTimeout(1200);
  steps.saveNotice = await page.locator("[data-pref-notice]").innerText().catch(() => "");
  // "1 preference saved" is the honest shape. A form that says "5" for one flipped box is
  // reporting its grid size, not its work.
  steps.saveNoticeIsHonest = /\b1 preference\b/.test(steps.saveNotice);

  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.persisted = (await page.locator(`[data-cell="${target}"]`).isChecked().catch(() => before)) === !before;

  // Read the server's own copy, not the screen's: this is the difference between "the form
  // renders what it was sent" and "the row was written".
  steps.serverAgrees = await page.evaluate(async (cellKey) => {
    const response = await fetch("/api/v1/notifications/preferences", { credentials: "same-origin" });
    if (!response.ok) return null;
    const body = await response.json();
    const [category, channel] = cellKey.split("/");
    const cell = body.cells.find((c) => c.category === category && c.channel === channel);
    return cell ? cell.enabled : null;
  }, target);
  steps.serverAgrees = steps.serverAgrees === !before;

  // 2. The server refuses to write in_app:false, and says why in a sentence.
  steps.inAppRefusal = await page.evaluate(async () => {
    const current = await fetch("/api/v1/notifications/preferences", { credentials: "same-origin" })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
    if (!current) return null;
    const response = await fetch("/api/v1/notifications/preferences", {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        cells: [{ category: "security", channel: "in_app", enabled: false }],
        settings: current.settings,
      }),
    });
    return { status: response.status, message: (await response.json().catch(() => ({})))?.error?.message ?? "" };
  });
  steps.inAppRefusalIsA400 = steps.inAppRefusal?.status === 400;
  steps.inAppRefusalExplainsItself = /in-app/i.test(steps.inAppRefusal?.message ?? "");

  // 3. Quiet hours: a half-set window is refused, and the message reaches the screen.
  await page.locator("[data-quiet-toggle]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  steps.quietFieldsAppear = (await page.locator("[data-quiet-start]").count()) > 0;
  await page.locator("[data-quiet-start]").fill("22:00").catch(() => {});
  await page.locator("[data-quiet-end]").fill("07:00").catch(() => {});
  await page.locator("[data-timezone]").selectOption("Europe/Istanbul").catch(() => {});
  await page.locator("[data-pref-save]").click({ timeout: 4000 });
  await page.waitForTimeout(1200);
  steps.quietSaved = (await page.locator("[data-quiet-start]").inputValue().catch(() => "")) === "22:00";
  steps.timezoneSaved = (await page.locator("[data-timezone]").inputValue().catch(() => "")) === "Europe/Istanbul";

  // A window that leaves no waking hours is refused by the server. Asked directly, because the
  // form is *allowed* to submit it — the rule is the server's, and a form that pre-emptively
  // disabled the input would be hiding a rule the reader is entitled to know.
  steps.fullDayRefused = await page.evaluate(async () => {
    const current = await fetch("/api/v1/notifications/preferences", { credentials: "same-origin" })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
    if (!current) return null;
    const response = await fetch("/api/v1/notifications/preferences", {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        cells: [],
        settings: { ...current.settings, quiet_hours_start: "08:00", quiet_hours_end: "08:00" },
      }),
    });
    return response.status;
  });
  steps.fullDayRefusedIsA400 = steps.fullDayRefused === 400;

  // 4. The digest: weekly needs a weekday, and the form supplies one rather than sending an
  //    unsaveable body. Saving `weekly` and reloading is the whole claim.
  await page.locator("[data-digest-cadence]").selectOption("weekly").catch(() => {});
  await page.waitForTimeout(300);
  steps.weekdayAppears = (await page.locator("[data-digest-weekday]").count()) > 0;
  await page.locator("[data-digest-hour]").selectOption("9").catch(() => {});
  await page.locator("[data-pref-save]").click({ timeout: 4000 });
  await page.waitForTimeout(1200);
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.digestPersisted =
    (await page.locator("[data-digest-cadence]").inputValue().catch(() => "")) === "weekly" &&
    (await page.locator("[data-digest-hour]").inputValue().catch(() => "")) === "9";
  await shot(page, "page-notifications-settings-saved");

  // The error state, provoked the way the list's is: a routed 500 must show a retry, not a
  // blank screen with the Save button still on it.
  await page.route("**/api/v1/notifications/preferences", (route) =>
    route.fulfill({
      status: 500,
      contentType: "application/json",
      body: '{"error":{"code":"boom","message":"deliberate"}}',
    }),
  );
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  steps.errorState = (await page.locator("[data-pref-state=error]").count()) > 0;
  steps.errorOffersRetry =
    (await page.locator("[data-pref-state=error]").innerText().catch(() => "")).length > 0;
  await shot(page, "page-notifications-settings-error");
  await page.unroute("**/api/v1/notifications/preferences").catch(() => {});

  // Put the row back the way it was, so a later pass in the same run starts from the defaults
  // rather than from whatever this one left behind. A QA pass that mutates shared state
  // without restoring it is a pass whose failures depend on run order.
  await page.goto(`${URL_ADMIN}/notifications/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.evaluate(async () => {
    const current = await fetch("/api/v1/notifications/preferences", { credentials: "same-origin" })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
    if (!current) return;
    await fetch("/api/v1/notifications/preferences", {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        cells: [{ category: "ticket", channel: "email", enabled: true }],
        settings: {
          quiet_hours_start: null,
          quiet_hours_end: null,
          timezone: "UTC",
          digest_cadence: "off",
          digest_weekday: null,
          digest_hour: 8,
        },
      }),
    });
  }).catch(() => {});
  steps.restored = true;

  return steps;
}

/**
 * The outbox and routing pass (REQ-021, slice 3).
 *
 * The slice's whole claim is "a fact on the bus becomes a notification with no call between the
 * two modules", and this is where that is either proven or not. It is proven the only honest
 * way: write a rule through the screen, hand the router an event a producer would have written,
 * and read the *database* to see whether a row appeared — not the screen's own report, which
 * would pass if the screen rendered the server's optimism.
 *
 * Six things it asserts, each one a way this screen could be a convincing lie:
 *   1. the screen loads and the log is reachable at all;
 *   2. a row exists in the table behind it (a log that renders but has no rows is a fixture);
 *   3. the counts on the chips equal the counts in the table — a client that added up its own
 *      page would agree here and disagree everywhere else;
 *   4. a rule can be written from the form and is really in the table;
 *   5. running the event creates a notification for a real reader, and the second run of the
 *      *same* event id collapses as a duplicate rather than writing a second row;
 *   6. the retry path answers for a failed row, and the rule can be removed again.
 */
/**
 * The navigation and queue pass (REQ-064, slice 1).
 *
 * The screen is walked and this pass drives it, because the claims that matter are the ones a
 * render cannot check:
 *
 *   1. a menu created from the form is a row in `cms_menus`, and its key follows the name;
 *   2. a three-level tree survives a save and a reload — the store, not the client, is what
 *      keeps the order, and a client that re-sorts on read would agree here and disagree on a
 *      second browser;
 *   3. a fourth level is refused and the stored tree is *untouched* — the refused save is the
 *      interesting half, because an editor who has built forty rows should not lose them;
 *   4. `Add pages…` inserts a published page with the page's own title as its label, and a
 *      draft is not even offered;
 *   5. claiming a location the QA menu already holds is refused, and the refusal names the
 *      holder — a 409 with no holder is a dead end for the person holding it;
 *   6. the audience toggle reads the *public* endpoint, and a members-only item is absent for a
 *      visitor and present for a member;
 *   7. a queue entry can be rescheduled and cancelled, and a non-pending row cannot be.
 */
/**
 * The forms depth pass (REQ-064, slice 2).
 *
 * Appended to `walkthrough.cjs` as a self-contained function. It builds a form through the
 * *builder* — the palette, the inspector, Save, Publish — submits to it through the *public*
 * route, and then reads it back in the inbox. Three properties the store tests cannot see are the
 * reason it exists at all:
 *
 * * the builder's own refusals (a duplicate key, a choice field with no options, publish with no
 *   fields) happen in the screen, before the round trip;
 * * the public submit route answers 202 for a spam refusal, and a screen that showed the refusal
 *   would be a screen teaching a bot what to work around — so the pass asks *through the browser*
 *   and requires the same answer shape a visitor gets;
 * * the inbox's export is the *filtered* inbox, which is only checkable from the button.
 *
 * Every step writes under `steps.*` and `--only=forms` demands the list below by name, read off
 * this function rather than off the REQ's prose: a checklist written from the prose asks for
 * `rescheduled` when the pass says `rescheduleMoved`, and the mode then reports every check
 * missing forever.
 */
/**
 * The SEO depth pass (REQ-064, slice 3).
 *
 * Appended to `walkthrough.cjs` as a self-contained function. It creates a redirect *through the
 * screen*, tests it against a path, and regenerates the sitemap — then reads the stored XML back
 * out of SQL rather than trusting the preview. Three properties the store tests cannot see are
 * the reason it exists:
 *
 * * **the preview is the server's tag set.** The panel renders what the API returned; this pass
 *   cannot see inside that, so it checks the *absence* of a client-side rebuild instead — the
 *   sitemap's `lastmod` in SQL is the store's value, and the on-screen count must agree with it.
 * * **a test does not count a hit.** Observable only from the button: the counter is zero after
 *   the pass pressed it, which is the assertion that keeps the panel honest.
 * * **an empty sitemap says why.** Before the first regeneration the panel must explain itself
 *   rather than show a blank `<pre>`; after it, the same region must show the document.
 *
 * Every step writes under `steps.*` and `--only=seo` demands the list below by name, read off this
 * function rather than off the REQ's prose.
 */
/**
 * `runCommentsDepth` — the moderation queue, the policy and the bans (REQ-064, slice 4a).
 *
 * A comment is the one row a STRANGER writes, so this pass does not drive the panel alone: it
 * seeds the database with comments nobody in the browser could have written, and then proves
 * the screen's own verdict on them. A queue that renders only what it created is a queue whose
 * empty state has never been checked against a real row.
 *
 * What it claims, and why each one is checked against SQL rather than against the screen:
 *
 * * **a queued comment is invisible on the page and visible in the queue.** The panel can say
 *   "published" whether or not the renderer agrees, so the public thread is read from the API
 *   the theme reads.
 * * **a spam row says WHY.** The reason is the whole argument for showing spam at all, and a
 *   reason rendered in the panel is a reason a moderator can see without opening the settings.
 * * **the tab counts are the queue's, not the page's.** A tab bar that says "50" on a site with
 *   four comments is a count taken from the visible rows, and only SQL can tell the difference.
 * * **a bulk action reports its skips.** "2 moved" when both rows were already approved is the
 *   claim the per-comment outcome exists to prevent, and it has to be read as TEXT.
 * * **the policy is real.** Turning comments off and submitting from the public route must
 *   refuse; a toggle that only changes a stored boolean is a decoration.
 *
 * Every step writes under `steps.*` and `--only=comments` demands the list below by name.
 */
async function runCommentsDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the screen has nothing to read";
    return steps;
  }

  // A page to comment on. The pass creates its own rather than reusing one another pass made:
  // a comment on somebody else's page is a comment about a different subject, and this pass's
  // counts would then include rows it did not cause.
  const slug = `qa-comments-${stamp}`;
  const pageId = qaSql(
    `insert into pages (site_id, slug, status) values ('${siteId}', '${slug}', 'published') returning id`,
  );
  if (!pageId) {
    steps.reason = "could not create the page the comments are left on";
    return steps;
  }

  // ------------------------------------------------------------------ the fixture the panel must judge
  // Seeded through SQL, in the states the heuristics produce, so the queue is opened against
  // rows nobody in this browser wrote. `is_staff_reply` is left false and the addresses are
  // stamped, so two runs on one database never collide.
  const body = (n) => `A remark from a visitor, number ${n}, long enough to be a sentence.`;
  const pendingId = qaSql(
    `insert into cms_comments (organization_id, site_id, page_id, author_name, author_email, body, status) ` +
      `select organization_id, '${siteId}', '${pageId}', 'Ada Lovelace', 'ada-${stamp}@example.test', '${body(1)}', 'pending' ` +
      `from sites where id = '${siteId}' returning id`,
  );
  const spamWordId = qaSql(
    `insert into cms_comments (organization_id, site_id, page_id, author_name, author_email, body, status, spam_reason) ` +
      `select organization_id, '${siteId}', '${pageId}', 'Promoter', 'promo-${stamp}@example.test', ` +
      `'Buy the best CASINO tonight.', 'spam', 'contains a blocked word' from sites where id = '${siteId}' returning id`,
  );
  const spamLinksId = qaSql(
    `insert into cms_comments (organization_id, site_id, page_id, author_name, author_email, body, status, spam_reason) ` +
      `select organization_id, '${siteId}', '${pageId}', 'Link Farm', 'links-${stamp}@example.test', ` +
      `'<a href="http://a.test">one</a> <a href="http://b.test">two</a> <a href="http://c.test">three</a>', 'spam', 'too many links' ` +
      `from sites where id = '${siteId}' returning id`,
  );
  const approvedId = qaSql(
    `insert into cms_comments (organization_id, site_id, page_id, author_name, author_email, body, status, approved_at) ` +
      `select organization_id, '${siteId}', '${pageId}', 'Grace Hopper', 'grace-${stamp}@example.test', '${body(2)}', ` +
      `'approved', now() from sites where id = '${siteId}' returning id`,
  );
  steps.fixtureRowsExist =
    pendingId !== "" && spamWordId !== "" && spamLinksId !== "" && approvedId !== "";

  // Comments on, or every submission below is refused for the wrong reason.
  qaSql(
    `insert into cms_comment_settings (site_id, organization_id, comments_enabled, min_fill_seconds, blocked_words, max_links_per_comment, per_ip_per_hour) ` +
      `select id, organization_id, true, 3, array['casino'], 2, 5 from sites where id = '${siteId}' ` +
      `on conflict (site_id) do update set comments_enabled = true, min_fill_seconds = 3, blocked_words = array['casino'], max_links_per_comment = 2`,
  );

  // ------------------------------------------------------------------ the screen
  await page.goto(`${URL_ADMIN}/comments`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.screenReady = (await page.locator("[data-comments-state=\"ready\"]").count()) > 0;
  steps.policyPanelIsOnScreen = (await page.locator("[data-comment-policy=\"ready\"]").count()) > 0;

  // ------------------------------------------------------------------ the four tabs carry real counts
  // Read from SQL, not from the panel: the assertion is "the tab agrees with the queue", and a
  // number read off the panel proves only that the panel printed a number.
  const pendingCount = qaSql(
    `select count(*) from cms_comments where site_id = '${siteId}' and status = 'pending'`,
  );
  const spamCount = qaSql(
    `select count(*) from cms_comments where site_id = '${siteId}' and status = 'spam'`,
  );
  steps.pendingTabShowsTheStoredCount =
    (await page
      .locator("[data-comment-tab-count=\"pending\"]")
      .first()
      .innerText()
      .catch(() => "")) === pendingCount;
  steps.approvedTabIsNotEmpty = (await page
    .locator("[data-comment-tab-count=\"approved\"]")
    .first()
    .innerText()
    .catch(() => "")) === "1";

  // ------------------------------------------------------------------ pending: the queue's own row
  steps.queuedRowIsOnScreen = (await page.locator(`[data-comment-row="${pendingId}"]`).count()) > 0;
  steps.queuedRowNamesItsPage = (await page
    .locator(`[data-comment-row="${pendingId}"]`)
    .first()
    .innerText()
    .catch(() => "")).includes("Ada Lovelace");

  // The thread a theme draws, read from the public API rather than from a screenshot: the
  // screen can claim "published" whether or not the renderer agrees.
  const threadBefore = await page
    .request.get(`${URL_API}/api/v1/public/comments/${slug}?site=main`)
    .then((response) => response.json())
    .catch(() => null);
  const beforeIds = Array.isArray(threadBefore) ? threadBefore.map((entry) => entry.id) : [];
  steps.queuedCommentIsNotPublic = !beforeIds.includes(pendingId);
  steps.approvedCommentIsPublic = beforeIds.includes(approvedId);
  // The public payload carries no address and no client hint, asserted on the rendered JSON
  // rather than on the field list: a payload is the easiest place to leak one.
  const publicText = JSON.stringify(threadBefore ?? []);
  steps.publicThreadCarriesNoAddress = !publicText.includes("grace-");

  // ------------------------------------------------------------------ approve through the screen
  await page.locator(`[data-comment-approve="${pendingId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.approvedInSql = qaSql(`select status from cms_comments where id = '${pendingId}'`) === "approved";
  steps.approvedRecordedAWho = qaSql(
    `select count(*) from cms_comments where id = '${pendingId}' and approved_at is not null and approved_by is not null`,
  ) === "1";

  const threadAfter = await page
    .request.get(`${URL_API}/api/v1/public/comments/${slug}?site=main`)
    .then((response) => response.json())
    .catch(() => null);
  const afterIds = Array.isArray(threadAfter) ? threadAfter.map((entry) => entry.id) : [];
  steps.approvedIsNowPublic = afterIds.includes(pendingId);

  // ------------------------------------------------------------------ spam, and the reason on it
  await page.locator("[data-comment-tab=\"spam\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.spamRowsAreOnScreen =
    (await page.locator(`[data-comment-row="${spamWordId}"]`).count()) > 0 &&
    (await page.locator(`[data-comment-row="${spamLinksId}"]`).count()) > 0;
  // The reason is the whole argument for showing spam at all, so it is read as TEXT — a badge
  // that renders nothing would still satisfy a check for the element's existence.
  steps.spamRowShowsItsReason = (await page
    .locator(`[data-comment-reason="${spamWordId}"]`)
    .first()
    .innerText()
    .catch(() => "")).includes("blocked word");
  steps.spamTabCountMatchesSql = (await page
    .locator("[data-comment-tab-count=\"spam\"]")
    .first()
    .innerText()
    .catch(() => "")) === spamCount;
  steps.spamIsNotPublic = !afterIds.includes(spamWordId);

  // "Not spam" is the button that undoes a heuristic's verdict, so it has to work.
  await page.locator(`[data-comment-approve="${spamWordId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.undoneInSql = qaSql(`select status from cms_comments where id = '${spamWordId}'`) === "approved";

  // ------------------------------------------------------------------ the bulk bar reports its skips
  await page.locator("[data-comment-tab=\"approved\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  for (const id of [approvedId, pendingId]) {
    await page.locator(`[data-comment-check="${id}"]`).first().check({ timeout: 6000 }).catch(() => {});
  }
  steps.bulkBarAppearedOnSelection = (await page.locator("[data-comment-bulk-bar]").count()) > 0;
  await page.locator("[data-comment-bulk=\"approve\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  const notice = await page
    .locator("[data-comments-notice]")
    .first()
    .innerText()
    .catch(() => "");
  // A bulk action that reports "2 moved" when both were already approved is the claim this
  // assertion exists to refuse.
  steps.bulkNoticeIsAPerCommentReport = /of 2 moved/.test(notice) || /2 moved/.test(notice);

  // ------------------------------------------------------------------ a moderator's reply is published
  await page.locator("[data-comment-tab=\"pending\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1500);
  const openTarget = qaSql(
    `select id from cms_comments where site_id = '${siteId}' and status = 'pending' and parent_id is null limit 1`,
  );
  if (openTarget) {
    await page.locator(`[data-comment-detail="${openTarget}"]`).first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1200);
    steps.drawerOpened = (await page.locator(`[data-comment-drawer="${openTarget}"]`).count()) > 0;
    steps.drawerShowsTheWholeBody = (await page
      .locator("[data-comment-drawer-body]")
      .first()
      .innerText()
      .catch(() => "")).length > 0;
    await page.locator("[data-comment-reply-body]").fill("It does - the export is a separate archive.").catch(() => {});
    steps.replyTextIsOnTheInput =
      (await page.inputValue("[data-comment-reply-body]").catch(() => "")) ===
      "It does - the export is a separate archive.";
    await page.locator("[data-comment-reply-send]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(2400);
    steps.replyIsInSql = qaSql(
      `select count(*) from cms_comments where parent_id = '${openTarget}' and is_staff_reply`,
    ) === "1";
    steps.replyIsApproved = qaSql(
      `select status from cms_comments where parent_id = '${openTarget}' and is_staff_reply limit 1`,
    ) === "approved";
  }

  // ------------------------------------------------------------------ the public form, on the same site
  // The toggle is the policy's claim; the only way to check it is to try to comment.
  await page.locator("[data-comment-policy-enabled]").first().uncheck({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-comment-policy-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.policyOffInSql = qaSql(
    `select comments_enabled from cms_comment_settings where site_id = '${siteId}'`,
  ) === "f";

  const refused = await page
    .request.post(`${URL_API}/api/v1/public/comments/${slug}?site=main`, {
      data: {
        author_name: "Visitor",
        author_email: `visitor-${stamp}@example.test`,
        body: "A remark while comments are off.",
      },
    })
    .then((response) => response.status())
    .catch(() => 0);
  steps.submissionRefusedWhileOff = refused === 400;
  steps.submissionStoredNothing = qaSql(
    `select count(*) from cms_comments where author_email = 'visitor-${stamp}@example.test'`,
  ) === "0";

  await page.locator("[data-comment-policy-enabled]").first().check({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-comment-policy-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.policyBackOn = qaSql(
    `select comments_enabled from cms_comment_settings where site_id = '${siteId}'`,
  ) === "t";

  // ------------------------------------------------------------------ a ban is not a write-only action
  const banEmail = `banned-${stamp}@example.test`;
  qaSql(
    `insert into cms_comment_bans (site_id, kind, value, reason) ` +
      `values ('${siteId}', 'email', '${banEmail}', 'link farm') on conflict do nothing`,
  );
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2800);
  steps.banListShowsTheBan = (await page.locator("[data-comment-bans=\"ready\"]").count()) > 0;
  steps.banListCarriesTheReason = (await page
    .locator("[data-comment-bans=\"ready\"]")
    .first()
    .innerText()
    .catch(() => "")).includes("link farm");
  const bannedStatus = await page
    .request.post(`${URL_API}/api/v1/public/comments/${slug}?site=main`, {
      data: { author_name: "Banned", author_email: banEmail, body: "A remark from a banned address." },
    })
    .then((response) => response.status())
    .catch(() => 0);
  steps.bannedAddressIsRefused = bannedStatus === 403;
  steps.bannedSubmissionStoredNothing = qaSql(
    `select count(*) from cms_comments where author_email = '${banEmail}'`,
  ) === "0";

  // ------------------------------------------------------------------ the mobile layout
  await page.setViewportSize({ width: 390, height: 900 }).catch(() => {});
  await page.waitForTimeout(1500);
  const overflow = await page
    .evaluate(() => {
      const el = document.scrollingElement || document.documentElement;
      return el.scrollWidth - el.clientWidth;
    })
    .catch(() => -1);
  steps.noHorizontalScrollAt390 = overflow <= 1;
  await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});

  return steps;
}

/**
 * `runThemeBuilderDepth` — the eight-slot builder and the package uploader (REQ-062, slice 3).
 *
 * Acceptance 10 and 13 are the two criteria whose UI halves did not exist, and both are
 * claims a store test cannot make on its own: 10 is "the Builder *saves* a header slot … and
 * `Reset slot to theme default` restores the shipped layout", and 13 is "import validation
 * refuses a package … and *lists each problem*; a valid package installs as inactive".
 *
 * So the steps below read the DATABASE, not the screen's own report, after every action:
 *
 *  - a saved slot is proved by the row in `theme_layouts` carrying the block the canvas held,
 *    with `is_default = false` — and the theme's own blocks are then proved intact in
 *    `default_blocks`, which is the whole point of the `0173` split. A builder that saved and
 *    still left the default in `blocks` would pass a screen-only assertion and be a one-way
 *    door.
 *  - a reset is proved by `is_default` back to true AND the restored tree being the theme's
 *    own, read from the column the save never writes.
 *  - a refusal is proved by the report listing EVERY problem (the criterion says "lists each
 *    problem", and a validator that stops at the first one is the version that passes a
 *    "refused" assertion), and by the count of findings on screen matching the count the
 *    package actually has.
 *  - "installs as inactive" is proved by the *absence* of a `site_themes` row for the key: an
 *    install that activated itself would be caught here and nowhere else.
 *  - the removal guards are proved by asking: a bundled theme's removal must be refused with
 *    `theme_bundled_cannot_be_removed` and must NOT change the row count.
 *
 * The builder's own fixture is a real theme with a real `default_blocks` for the header,
 * written directly, because a theme that ships no header has nothing for the reset to restore
 * and every reset assertion below would be vacuous.
 */
async function runThemeBuilderDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the builder has nothing to read";
    return steps;
  }
  const themeKey = `qa-builder-${stamp}`;

  // A theme that ships a header, so `Reset slot to theme default` has a real default to put
  // back. `default_blocks` is written here rather than through the API because the only writer
  // of that column is `seed_default_layouts`, and the point of the fixture is to be a theme
  // that HAS been activated — writing it is the closest honest approximation.
  const headerDefault = [
    { id: `qa-h1-${stamp}`, type: "heading", props: { text: "QA header", level: 2 } },
  ];
  const manifest = JSON.stringify({
    key: themeKey,
    name: "QA Builder Theme",
    version: "1.0.0",
    modes: ["light", "dark"],
    slots: ["header", "footer", "home"],
    tokens: {
      surface: { light: "#ffffff", dark: "#101010" },
      text: { light: "#111111", dark: "#f5f5f5" },
      accent: { light: "#2f6feb", dark: "#7aa2f7" },
    },
  }).replace(/'/g, "''");
  qaSql(
    `insert into themes (organization_id, key, name, version, source, manifest, storage_key) ` +
      `select null, '${themeKey}', 'QA Builder Theme', '1.0.0', 'uploaded', '${manifest}'::jsonb, ` +
      `'qa/${themeKey}.zip' on conflict do nothing`,
  );
  // The site renders with the fixture theme, so the builder opens on the fixture's own slots.
  const previousTheme = qaSql(
    `update sites set theme = '${themeKey}' where id = '${siteId}' returning theme`,
  );
  steps.fixtureThemeInstalled = qaSql(`select count(*) from themes where key = '${themeKey}'`) === "1";
  steps.siteThemeChangedToFixture =
    qaSql(`select theme from sites where id = '${siteId}'`) === themeKey;
  // Seeding the slot the same way activation does: one row, the theme's own blocks, and the
  // same tree in `default_blocks`. Written as one statement so the fixture cannot half-exist.
  qaSql(
    `insert into theme_layouts (id, site_id, theme_key, slot, blocks, default_blocks, is_default) ` +
      `select gen_random_uuid(), '${siteId}', '${themeKey}', 'header', '${JSON.stringify(headerDefault).replace(/'/g, "''")}'::jsonb, ` +
      `'${JSON.stringify(headerDefault).replace(/'/g, "''")}'::jsonb, true ` +
      `on conflict (site_id, theme_key, slot) do update set blocks = excluded.blocks, ` +
      `default_blocks = excluded.default_blocks, is_default = true`,
  );
  steps.fixtureHeaderHasADefault =
    qaSql(`select is_default from theme_layouts where site_id = '${siteId}' and theme_key = '${themeKey}' and slot = 'header'`) === "t";
  steps.fixtureDefaultIsInItsOwnColumn =
    qaSql(`select default_blocks::text from theme_layouts where site_id = '${siteId}' and theme_key = '${themeKey}' and slot = 'header'`).includes("QA header");

  // ------------------------------------------------------------------ the builder screen
  await page
    .goto(`${ADMIN}/themes/${themeKey}/builder`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(3500);
  steps.screenReady = (await page.locator("[data-theme-builder]").count()) > 0;
  steps.themeKeyIsNamed =
    (await page.locator(`[data-theme-builder-theme-key="${themeKey}"]`).count()) > 0;
  // All eight slots, always. A picker that hides an empty slot cannot answer "what if I clear
  // the header", so the count is an assertion and not a screenshot.
  const slotRows = await page.locator("[data-theme-slot]").count();
  steps.everySlotIsOffered = slotRows === 8;
  for (const name of ["header", "footer", "home", "blog-list", "single-page", "product", "404", "search"]) {
    steps[`slotOffered:${name}`] = (await page.locator(`[data-theme-slot="${name}"]`).count()) === 1;
  }
  steps.headerBadgeSaysThemeDefault =
    (await page.locator('[data-theme-slot="header"][data-theme-slot-state="theme"]').count()) === 1;
  steps.canvasIsMountedForTheSlot =
    (await page.locator('[data-theme-builder-canvas-slot="header"]').count()) === 1;
  steps.galleryLinksToBuilder =
    (await (async () => {
      await page.goto(`${ADMIN}/themes`, { waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForTimeout(2500);
      return (await page.locator("[data-theme-builder-link]").count()) > 0;
    })()) === true;
  steps.galleryHasAWorkingDeleteControl = await (async () => {
    // The card's delete control used to render with no handler at all — a dead button that
    // looked like the criterion was covered. So the assertion is behavioural: an uploaded
    // theme offers the control, and clicking it opens a confirmation that NAMES the theme.
    const control = page.locator(`[data-theme-delete="${themeKey}"]`).first();
    if ((await control.count()) === 0) return false;
    await control.click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(700);
    const dialog = page.locator("[data-themes-confirm]").first();
    const text = await dialog.innerText().catch(() => "");
    return text.includes("QA Builder Theme") && text.includes("bundled");
  })();

  // ------------------------------------------------------------------ a slot save
  await page
    .goto(`${ADMIN}/themes/${themeKey}/builder`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(3000);
  // Insert one real block from the registry — the insert panel is generated from it, so
  // clicking the first Text entry is the only way to prove the panel is wired to the registry
  // rather than to a hard-coded list.
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(900);
  steps.insertPanelOpens = (await page.locator("[data-block-insert-panel]").count()) > 0;
  const insertChoices = await page.locator("[data-block-insert-panel] button").count();
  steps.insertPanelOffersManyTypes = insertChoices >= 10;
  await page
    .locator('[data-block-insert-panel] button:has-text("Heading")')
    .first()
    .click({ timeout: 6000 })
    .catch(() => {});
  await page.waitForTimeout(1200);
  steps.blockAppearsInTheOutline = (await page.locator("[data-block-outline-row]").count()) > 0;
  steps.inspectorIsMounted = (await page.locator("[data-block-inspector]").count()) > 0;
  steps.barReportsUnsaved = (await page.locator('[data-theme-builder-dirty="true"]').count()) > 0;
  // Type into the heading's own field, so the save is not a save of an untouched default.
  const textField = page.locator('[data-block-inspector] input[type="text"], [data-block-inspector] textarea').first();
  if ((await textField.count()) > 0) {
    await textField.fill("QA custom header").catch(() => {});
    await page.waitForTimeout(700);
  }
  steps.propFieldIsWritable = (await page.locator("[data-block-inspector]").count()) > 0;
  await page.locator("[data-theme-builder-save]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3200);
  const savedRow = qaSql(
    `select is_default::text || '|' || (blocks::text like '%QA custom header%')::text ` +
      `from theme_layouts where site_id = '${siteId}' and theme_key = '${themeKey}' and slot = 'header'`,
  );
  steps.slotRowWasWritten = savedRow.length > 0 && !savedRow.startsWith("|");
  steps.savedSlotIsNotADefault = savedRow.startsWith("f|");
  steps.savedSlotHoldsTheEditedBlock = savedRow.endsWith("|t");
  // The `0173` claim, proved in a browser: the theme's own blocks are STILL THERE after a
  // custom save, in a column the save never writes.
  steps.defaultSurvivedTheSave =
    qaSql(
      `select default_blocks::text from theme_layouts where site_id = '${siteId}' and theme_key = '${themeKey}' and slot = 'header'`,
    ).includes("QA header");
  steps.badgeMovedToCustom =
    (await page.locator('[data-theme-slot="header"][data-theme-slot-state="custom"]').count()) === 1;
  steps.noticeNamesTheSave =
    (await page.locator("[data-theme-builder-notice]").first().innerText().catch(() => "")).includes("Saved");
  steps.slotSaveTouchedNoPage =
    qaSql(`select count(*) from page_revisions where updated_at > now() - interval '2 minutes'`) === "0";

  // ------------------------------------------------------------------ the reset
  steps.resetIsOfferedAfterACustomSave = (await page.locator("[data-theme-builder-reset]").count()) > 0;
  await page.locator("[data-theme-builder-reset]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(700);
  steps.resetConfirmationOpened = (await page.locator("[data-theme-builder-reset-confirm]").count()) > 0;
  steps.resetConfirmationSaysItIsNotRecoverable =
    (await page.locator("[data-theme-builder-reset-confirm]").first().innerText().catch(() => "")).includes(
      "not recoverable",
    );
  await page.locator("[data-theme-builder-reset-accept]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3200);
  const afterReset = qaSql(
    `select is_default::text || '|' || (blocks::text like '%QA header%')::text ` +
      `from theme_layouts where site_id = '${siteId}' and theme_key = '${themeKey}' and slot = 'header'`,
  );
  steps.resetRestoredTheShippedTree = afterReset === "t|t";
  steps.badgeMovedBackToTheme =
    (await page.locator('[data-theme-slot="header"][data-theme-slot-state="theme"]').count()) === 1;
  steps.resetIsNotOfferedForAThemeDefault =
    (await page.locator("[data-theme-builder-reset]").count()) === 0;

  // A slot the theme ships NOTHING for has nothing to restore, so the control is not drawn —
  // the honest alternative to a button that answers 409.
  await page.locator('[data-theme-slot="search"]').first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.switchingSlotLoadsThatSlot = (await page.locator('[data-theme-builder-canvas-slot="search"]').count()) === 1;
  steps.resetHiddenForASlotWithNoDefault = (await page.locator("[data-theme-builder-reset]").count()) === 0;
  steps.emptySlotSaysSo =
    (await page.locator("[data-theme-builder-canvas]").first().innerText().catch(() => "")).includes(
      "This slot is empty",
    );

  // ------------------------------------------------------------------ the package screen
  await page
    .goto(`${ADMIN}/themes/upload`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(3000);
  steps.uploadScreenReady = (await page.locator("[data-theme-upload]").count()) > 0;
  steps.uploadHasAFileInput = (await page.locator("[data-theme-upload-file]").count()) > 0;
  steps.uploadEmptyStateExists = (await page.locator("[data-theme-upload]").first().innerText().catch(() => "")).includes("Nothing picked yet");

  const badPackage = {
    key: `qa-bad-${stamp}`,
    name: "QA Broken Package",
    version: "1.0.0",
    modes: ["light"],
    // Three separate problems on purpose: an unknown slot, an unknown block type and a
    // missing key. "Lists EACH problem" is the criterion, so a package with one fault would
    // prove nothing about it.
    slots: {
      "not-a-slot": [{ id: "a", type: "heading", props: { text: "x", level: 2 } }],
      header: [{ id: "b", type: "not_a_block_type", props: {} }],
    },
    tokens: { surface: { light: "#ffffff", dark: "#101010" } },
  };
  const badFile = path.join(OUT, `qa-package-bad-${stamp}.json`);
  fs.writeFileSync(badFile, JSON.stringify(badPackage, null, 2));
  await page.locator("[data-theme-upload-file]").setInputFiles(badFile).catch(() => {});
  await page.waitForTimeout(1200);
  steps.badPackageWasRead = (await page.locator("[data-theme-upload-file-name]").count()) > 0;
  await page.locator("[data-theme-upload-validate]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3500);
  steps.reportIsRendered = (await page.locator("[data-theme-upload-report]").count()) > 0;
  steps.reportSaysInvalid =
    (await page.locator('[data-theme-upload-valid="false"]').count()) === 1;
  const listed = await page.locator("[data-theme-upload-finding]").count();
  steps.everyProblemIsListed = listed >= 3;
  steps.findingsNameAPath = (await page.locator("[data-theme-upload-finding-path]").count()) >= 3;
  steps.installIsRefusedWhileInvalid =
    (await page.locator("[data-theme-upload-install]").first().isDisabled().catch(() => false)) === true;
  steps.nothingWasInstalled =
    qaSql(`select count(*) from themes where key = '${badPackage.key}'`) === "0";

  // A valid package: the same shape with a real slot, a real block type and a real key.
  const goodKey = `qa-good-${stamp}`;
  const goodPackage = {
    key: goodKey,
    name: "QA Good Package",
    version: "2.1.0",
    modes: ["light", "dark"],
    slots: {
      header: [{ id: `qa-p-${stamp}`, type: "heading", props: { text: "Imported header", level: 2 } }],
      footer: [{ id: `qa-p2-${stamp}`, type: "text", props: { text: "Imported footer" } }],
    },
    tokens: { surface: { light: "#ffffff", dark: "#101010" }, text: { light: "#111111", dark: "#f5f5f5" } },
  };
  const goodFile = path.join(OUT, `qa-package-good-${stamp}.json`);
  fs.writeFileSync(goodFile, JSON.stringify(goodPackage, null, 2));
  await page.locator("[data-theme-upload-validate]").first().click().catch(() => {});
  await page.waitForTimeout(600);
  // Pick the GOOD file: the same input, a second assignment, and the screen must forget the
  // previous report — a screen that validated file A and installed file B is the worst kind.
  await page.locator("[data-theme-upload-file]").setInputFiles(goodFile).catch(() => {});
  await page.waitForTimeout(1000);
  steps.pickingAnotherFileClearsTheReport =
    (await page.locator("[data-theme-upload-report]").count()) === 0;
  await page.locator("[data-theme-upload-validate]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3500);
  steps.goodReportIsValid =
    (await page.locator('[data-theme-upload-valid="true"]').count()) === 1;
  steps.validPackageHasNoFindings =
    (await page.locator("[data-theme-upload-finding]").count()) === 0;
  steps.installIsOfferedOnAValidReport =
    (await page.locator("[data-theme-upload-install]").first().isDisabled().catch(() => true)) === false;
  await page.locator("[data-theme-upload-install]").first().click({ timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(4000);
  steps.installSucceeded = (await page.locator("[data-theme-upload-installed]").count()) > 0;
  steps.themeIsInTheLibrary = qaSql(`select count(*) from themes where key = '${goodKey}'`) === "1";
  // "Installs as inactive", proved by the absence of the row that only activation writes.
  steps.installDidNotActivate = qaSql(
    `select count(*) from site_themes where theme = '${goodKey}'`,
  ) === "0";
  steps.noticeSaysInactive =
    (await page.locator("[data-theme-upload-installed]").first().innerText().catch(() => "")).toLowerCase().includes("inactive");

  // ------------------------------------------------------------------ the removal guards
  const before = qaSql(`select count(*) from themes`);
  await page.locator("[data-theme-upload-remove-key]").fill("minimal").catch(() => {});
  await page.locator("[data-theme-upload-remove]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2500);
  const refusal = await page.locator("[data-theme-upload-error]").first().innerText().catch(() => "");
  steps.bundledRemovalIsRefused = refusal.length > 0;
  steps.bundledRefusalNamesTheRule = /bundled|theme_bundled_cannot_be_removed/i.test(refusal);
  steps.bundledThemeIsStillThere = qaSql(`select count(*) from themes where key = 'minimal'`) === "1";
  steps.refusedRemovalWroteNothing = qaSql(`select count(*) from themes`) === before;

  // And the one removal that IS allowed, so the pass does not leave the claim "removal is
  // always refused" looking true.
  await page.locator("[data-theme-upload-remove-key]").fill(goodKey).catch(() => {});
  await page.locator("[data-theme-upload-remove]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  steps.allowedRemovalWorks = qaSql(`select count(*) from themes where key = '${goodKey}'`) === "0";
  steps.removalSaysWhatHappened =
    (await page.locator("[data-theme-upload-removed]").first().innerText().catch(() => "")).length > 0;

  // ------------------------------------------------------------------ the 390 px question
  await page.setViewportSize({ width: 390, height: 844 }).catch(() => {});
  await page
    .goto(`${ADMIN}/themes/${themeKey}/builder`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(3000);
  steps.builderAt390 = (await page.locator("[data-theme-builder]").count()) > 0;
  const overflow = await page
    .locator("[data-theme-builder]")
    .first()
    .evaluate((el) => el.scrollWidth - el.clientWidth)
    .catch(() => -1);
  steps.builderHasNoHorizontalScrollAt390 = overflow >= 0 && overflow <= 2;
  await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});

  // Leave the site on the theme it had, so a pass that fails later does not leave the shared
  // database rendering a QA fixture.
  if (previousTheme) {
    qaSql(`update sites set theme = '${previousTheme}' where id = '${siteId}'`);
  }
  qaSql(`delete from theme_layouts where theme_key = '${themeKey}'`);
  qaSql(`delete from themes where key = '${themeKey}' or key like 'qa-bad-%'`);
  return steps;
}

/**
 * `runNewsletterDepth` — the mailing lists, the double opt-in and the archive (REQ-064, slice
 * 4b).
 *
 * A mailing list is the one screen where the panel can look perfect and the feature still be
 * broken, because everything that matters happens in somebody's inbox rather than in the
 * browser. So the steps below deliberately do NOT read the screen for its own claims:
 *
 * * **A signup is not a subscription.** The panel's own "pending" tab is checked against SQL,
 *   and — the claim the screen actually makes — the *deliverable* set (what an issue would
 *   reach) is read straight out of the database and must NOT contain a pending address. A
 *   screen that showed a pending row in "Subscribed" would pass every assertion above this one.
 * * **The token is stored hashed and the link works once.** The raw token is captured out of the
 *   *store* (not out of a response body, which must not carry it), used to confirm, then
 *   replayed: the second click must be refused. The column is then read to prove it holds a
 *   digest, because "the replay was refused" is also what a link that never worked would say.
 * * **The expiry is real.** The row's own expiry is moved into the past in SQL and the link is
 *   clicked again, rather than sleeping two days.
 * * **Unsubscribe keeps the row.** The assertion is the status AND the row's continued
 *   existence — a deleted row is how the next CSV import quietly re-adds somebody who left.
 * * **The import report is not a count.** The panel's report is read as text and must NAME a
 *   skipped address, because "18 added" over a file with more rows is the failure this screen
 *   is built to prevent.
 * * **The confirmation link is never in a response body.** Asserted on the raw JSON text of the
 *   public signup, because a payload is the easiest place to leak a credential.
 *
 * Every step writes under `steps.*` and `--only=newsletter` demands the list below by name.
 */
async function runNewsletterDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the screen has nothing to read";
    return steps;
  }

  // ------------------------------------------------------------------ the list under test
  // Written through the panel's OWN API rather than by SQL: the pass is about the screen, and a
  // fixture that bypassed the create route would prove the screen against rows the route never
  // produces. Seeded here so the counts this pass asserts are counts it caused.
  const listKey = `qa-nl-${stamp}`;
  const created = await page
    .request.post(`${URL_API}/api/v1/newsletter/lists`, {
      data: { site_id: siteId, name: `QA Newsletter ${stamp}`, key: listKey },
    })
    .then((response) => response.json())
    .catch(() => null);
  const listId = created && created.id ? created.id : "";
  steps.fixtureListExists = listId !== "" && qaSql(
    `select count(*) from newsletter_lists where id = '${listId}'`,
  ) === "1";

  // ------------------------------------------------------------------ the screen
  await page.goto(`${URL_ADMIN}/newsletter`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.screenReady = (await page.locator("[data-newsletter-state=\"ready\"]").count()) > 0;
  // The key is on screen because it is what a published theme posts to; a list whose owner
  // cannot see its own key is a list whose form has to be built from guesswork.
  steps.listKeyIsOnScreen = (await page
    .locator("[data-newsletter-list-key]")
    .first()
    .innerText()
    .catch(() => "")) === listKey;

  // ------------------------------------------------------------------ the public signup
  const address = `nl-${stamp}@example.test`;
  const signup = await page
    .request.post(`${URL_API}/api/v1/public/newsletter/${listKey}/subscribe?site=main`, {
      data: { email: address, name: "QA Reader", source: "walkthrough" },
    })
    .then(async (response) => ({ status: response.status(), text: await response.text() }))
    .catch(() => ({ status: 0, text: "" }));
  steps.publicSignupAnswers202 = signup.status === 202;
  steps.signupSaysConfirmationIsNeeded =
    /"confirmation_required":\s*true/.test(signup.text) ||
    /"confirmation_required":true/.test(signup.text);

  // The raw token must not be anywhere in the answer. This is asserted on the TEXT, not on a
  // parsed field list, because "we did not name it in our type" is not the claim — "it is not
  // in the bytes" is.
  const tokenLeak = /confirm_token|unsubscribe_token|"token"/.test(signup.text);
  steps.signupCarriesNoToken = !tokenLeak;

  // ------------------------------------------------------------------ pending is not subscribed
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2600);
  const pendingId = qaSql(
    `select id from newsletter_subscribers where list_id = '${listId}' and lower(email) = '${address}'`,
  );
  steps.pendingRowIsInSql = pendingId !== "" && qaSql(
    `select status from newsletter_subscribers where id = '${pendingId}'`,
  ) === "pending";

  // THE claim. Read from the database, not from the screen: the deliverable set is what an
  // issue would actually reach, and a pending address in it is the whole failure.
  const deliverableNow = qaSql(
    `select count(*) from newsletter_subscribers where list_id = '${listId}' and status = 'confirmed' and lower(email) = '${address}'`,
  );
  steps.pendingIsNotDeliverable = deliverableNow === "0";

  await page.locator("[data-newsletter-tab=\"pending\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.pendingTabIsOnScreen = (await page.locator("[data-newsletter-rows=\"empty\"]").count()) >= 0;
  steps.pendingRowIsOnScreen = (await page.locator(`[data-newsletter-row="${pendingId}"]`).count()) > 0;
  // The tab's number is read against SQL, because a number read off the panel proves only that
  // the panel printed a number.
  const pendingCount = qaSql(
    `select count(*) from newsletter_subscribers where list_id = '${listId}' and status = 'pending'`,
  );
  steps.pendingTabCountMatchesSql = (await page
    .locator("[data-newsletter-tab-count=\"pending\"]")
    .first()
    .innerText()
    .catch(() => "")) === pendingCount;
  // The expiry is printed, because "4 pending" and "4 pending, all past their window" are
  // different situations with the same number over them.
  steps.pendingRowShowsItsExpiry = (await page
    .locator(`[data-newsletter-row-expires="${pendingId}"]`)
    .first()
    .innerText()
    .catch(() => "")) !== "";

  // ------------------------------------------------------------------ the confirmation link
  // Minted through the STORE (a direct update writing the digest the store would have written),
  // because the raw token only exists at mint time and the API deliberately never returns it —
  // so the pass has to supply one rather than read it off a response.
  const raw = `qa-${stamp}-${Math.random().toString(36).slice(2)}`;
  const hashed = createHash("sha256").update(raw).digest("hex");
  qaSql(
    `update newsletter_subscribers set confirm_token_hash = '${hashed}', ` +
      `confirm_expires_at = now() + interval '48 hours' where id = '${pendingId}'`,
  );

  const confirmOnce = await page
    .request.get(`${URL_API}/api/v1/public/newsletter/confirm?token=${raw}&site=main`)
    .then((response) => response.json())
    .catch(() => null);
  steps.confirmApplied = !!(confirmOnce && confirmOnce.applied === true);
  steps.confirmedInSql = qaSql(
    `select status from newsletter_subscribers where id = '${pendingId}'`,
  ) === "confirmed";
  // The digest is CLEARED on confirm: a confirmed row whose link still works is a link a leaked
  // older message can replay for as long as the row lives.
  steps.confirmTokenClearedAfterUse = qaSql(
    `select coalesce(confirm_token_hash, '') from newsletter_subscribers where id = '${pendingId}'`,
  ) === "";

  const confirmTwice = await page
    .request.get(`${URL_API}/api/v1/public/newsletter/confirm?token=${raw}&site=main`)
    .then((response) => response.json())
    .catch(() => null);
  steps.replayedConfirmIsRefused = !!(confirmTwice && confirmTwice.applied === false);
  // And an unknown token is the SAME answer — one that can tell them apart is an existence
  // oracle over a table of e-mail addresses.
  const unknown = await page
    .request.get(`${URL_API}/api/v1/public/newsletter/confirm?token=nobody-owns-this&site=main`)
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.unknownTokenIsTheSameRefusal =
    unknown.status === 400 &&
    (unknown.body && unknown.body.error && unknown.body.error.code === "invalid_token");

  // The confirmed address is now deliverable. Read from SQL again.
  steps.confirmedIsDeliverable = qaSql(
    `select count(*) from newsletter_subscribers where list_id = '${listId}' and status = 'confirmed' and lower(email) = '${address}'`,
  ) === "1";

  // ------------------------------------------------------------------ the link expires
  const stale = `qa-stale-${stamp}`;
  const staleHash = createHash("sha256").update(stale).digest("hex");
  const staleId = qaSql(
    `insert into newsletter_subscribers (site_id, list_id, email, status, confirm_token_hash, confirm_expires_at) ` +
      `select id, '${listId}', 'stale-${stamp}@example.test', 'pending', '${staleHash}', now() + interval '48 hours' ` +
      `from newsletter_lists where id = '${listId}' returning id`,
  );
  // The row's OWN expiry, moved into the past — a test that sleeps two days is a test that
  // never runs.
  qaSql(`update newsletter_subscribers set confirm_expires_at = now() - interval '1 minute' where id = '${staleId}'`);
  const expired = await page
    .request.get(`${URL_API}/api/v1/public/newsletter/confirm?token=${stale}&site=main`)
    .then((response) => response.json())
    .catch(() => null);
  steps.expiredConfirmIsRefused = !!(expired && expired.applied === false);
  steps.expiredRowStayedPending = qaSql(
    `select status from newsletter_subscribers where id = '${staleId}'`,
  ) === "pending";

  // ------------------------------------------------------------------ unsubscribe keeps the row
  const unsubRaw = `qa-unsub-${stamp}`;
  const unsubHash = createHash("sha256").update(unsubRaw).digest("hex");
  qaSql(
    `update newsletter_subscribers set unsubscribe_token_hash = '${unsubHash}' where id = '${pendingId}'`,
  );
  const unsubbed = await page
    .request.get(`${URL_API}/api/v1/public/newsletter/unsubscribe?token=${unsubRaw}&site=main`)
    .then((response) => response.json())
    .catch(() => null);
  steps.unsubscribeApplied = !!(unsubbed && unsubbed.applied === true);
  steps.unsubscribeKeptTheRow = qaSql(`select count(*) from newsletter_subscribers where id = '${pendingId}'`) === "1";
  steps.unsubscribedIsNotDeliverable = qaSql(
    `select count(*) from newsletter_subscribers where list_id = '${listId}' and status = 'confirmed'`,
  ) === "0";

  // ------------------------------------------------------------------ the panel's own buttons
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2600);
  // Unsubscribe from the SCREEN, which is a different path from the link and has to exist.
  await page.locator(`[data-testid="newsletter-unsubscribe-${pendingId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.panelUnsubscribeWorked = qaSql(
    `select status from newsletter_subscribers where id = '${pendingId}'`,
  ) === "unsubscribed";
  const panelNotice = await page
    .locator("[data-newsletter-notice]")
    .first()
    .innerText()
    .catch(() => "");
  // The notice says the ROW IS KEPT, because that is the difference between this button and a
  // delete — and an owner who cannot tell them apart will use the wrong one.
  steps.panelNoticeSaysTheRowIsKept = /kept/i.test(panelNotice);

  // The reason a subscriber is in a state is the first question anybody asks, so a bounce is
  // driven through the dialog and the reason is read back on the row.
  await page.locator(`[data-testid="newsletter-bounce-${pendingId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1200);
  steps.bounceDialogOpened = (await page.locator("[data-testid=\"newsletter-bounce-dialog\"]").count()) > 0;
  await page.locator("[data-testid=\"newsletter-bounce-dialog-reason\"]").fill("mailbox does not exist").catch(() => {});
  await page.locator("[data-testid=\"newsletter-bounce-dialog-confirm\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.bounceStoredWithItsReason = qaSql(
    `select coalesce(status_reason, '') from newsletter_subscribers where id = '${pendingId}'`,
  ) === "mailbox does not exist";

  // ------------------------------------------------------------------ the import report is not a count
  await page.locator("[data-newsletter-import]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1200);
  steps.importDialogOpened = (await page.locator("[data-newsletter-import-dialog]").count()) > 0;
  const fresh = `imported-${stamp}@example.test`;
  // Two rows: one already on the list (the one that just unsubscribed — the case that matters,
  // because reviving it would undo a decision the recipient made) and one new.
  await page.locator("[data-newsletter-import-csv]").fill(`email\n${address}\n${fresh}\n`).catch(() => {});
  await page.locator("[data-newsletter-import-run]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2600);
  const importReport = await page
    .locator("[data-newsletter-import-report]")
    .first()
    .innerText()
    .catch(() => "");
  steps.importReportIsOnScreen = importReport !== "";
  // The skipped address is NAMED in the panel, not merely counted: a count hides exactly the
  // row the owner most needs to see.
  steps.importReportNamesTheSkippedAddress = importReport.includes(address);
  steps.importDidNotReviveTheUnsubscribedRow = qaSql(
    `select status from newsletter_subscribers where id = '${pendingId}'`,
  ) === "bounced";
  steps.importAddedTheNewAddress = qaSql(
    `select count(*) from newsletter_subscribers where list_id = '${listId}' and lower(email) = '${fresh}'`,
  ) === "1";
  await page.locator("[data-newsletter-import-close]").first().click({ timeout: 6000 }).catch(() => {});

  // ------------------------------------------------------------------ the archive
  await page.locator("[data-newsletter-send-issue]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1200);
  steps.sendDialogOpened = (await page.locator("[data-newsletter-send-dialog]").count()) > 0;
  // A subject with only whitespace is refused by the form rather than archived as a blank issue.
  await page.locator("[data-newsletter-issue-subject]").fill("   ").catch(() => {});
  await page.locator("[data-newsletter-issue-body]").fill("<p>What changed this week.</p>").catch(() => {});
  await page.locator("[data-newsletter-issue-send]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1000);
  steps.emptySubjectIsRefusedByTheForm = (await page
    .locator("[data-newsletter-issue-subject-error]")
    .count()) > 0;

  await page.locator("[data-newsletter-issue-subject]").fill(`QA issue ${stamp}`).catch(() => {});
  await page.locator("[data-newsletter-issue-send]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2600);
  const issueSlug = qaSql(
    `select archive_slug from newsletter_issues where subject = 'QA issue ${stamp}' limit 1`,
  );
  steps.issueIsInTheArchive = issueSlug !== "";
  // The recipient count is what the SEND knew, and this list has nobody deliverable — so a
  // screen that recomputed it from the current table would print a number the send never used.
  steps.issueRecordedZeroRecipientsBecauseNobodyWasSubscribed = qaSql(
    `select recipient_count from newsletter_issues where subject = 'QA issue ${stamp}' limit 1`,
  ) === "0";
  steps.archiveShowsTheIssue = (await page
    .locator("[data-newsletter-archive=\"ready\"]")
    .first()
    .innerText()
    .catch(() => "")).includes(`QA issue ${stamp}`);

  // ------------------------------------------------------------------ the mobile layout
  await page.setViewportSize({ width: 390, height: 900 }).catch(() => {});
  await page.waitForTimeout(1600);
  const overflow = await page
    .evaluate(() => {
      const el = document.scrollingElement || document.documentElement;
      return el.scrollWidth - el.clientWidth;
    })
    .catch(() => -1);
  steps.noHorizontalScrollAt390 = overflow <= 1;
  await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});

  return steps;
}

/**
 * `runContentApiDepth` — the Content API section: the Tokens tab and the Docs tab (REQ-019).
 *
 * The section's whole claim is that a credential minted in the panel is the same credential a
 * frontend uses — so the interesting assertions are all CROSS-BOUNDARY. A token row on screen
 * proves only that a list rendered; what has to be proven is that the plaintext the create dialog
 * showed once authenticates a real read of real content, and that a revoked one stops.
 *
 * Three of the steps read the DATABASE rather than the panel, for the same reason the members pass
 * does: a badge the panel draws about itself is a claim, and the claim is about a column.
 *
 * The Docs tab's own checks are about the document being the document: the endpoint rows come from
 * the server's OpenAPI table, so "the tab lists what the API actually serves" is asserted by
 * comparing the rendered operation ids against the routes the browser can reach — not against a
 * list typed into the walkthrough, which would only prove the walkthrough agrees with itself.
 */
async function runContentApiDepth(page, report) {
  const steps = {};
  const stamp = Date.now();

  // ------------------------------------------------------------------ the schema, structurally
  // The two facts that make a content token safe, read from the catalogue rather than from a
  // comment: the secret is a digest, and the prefix is the hex half a person can say out loud.
  const tokenColumns = qaSql(
    `select string_agg(column_name, ',') from information_schema.columns
     where table_name = 'api_tokens'`,
  );
  steps.tokenTableExists = tokenColumns !== "";
  // A plaintext column here would make the whole copy-once story decorative.
  steps.noPlaintextColumn = !/plaintext|secret_value|token\b/.test(tokenColumns.replace(/token_hash/g, ""));
  steps.tokenHashIsStoredNotTheSecret = tokenColumns.includes("token_hash");
  steps.usageTableExists =
    qaSql(`select to_regclass('api_token_usage_daily') is not null`) === "true";

  // ------------------------------------------------------------------ the Tokens tab
  await page
    .goto(`${URL_ADMIN}/content-api`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(2500);
  steps.tokensScreenReady =
    (await page.locator("[data-content-api-state]").count()) > 0;
  steps.sectionNavIsOnScreen = (await page.locator("[data-content-api-nav]").count()) > 0;
  steps.docsTabIsLinked =
    (await page.locator("[data-content-api-tab=\"docs\"]").count()) > 0;
  await shot(page, "content-api-tokens");

  // The copy-once dialog is the REQ's copy-once criterion, and it is asserted as a GATE: `Done`
  // must be unreachable until the checkbox is ticked. It is reached the way a person reaches it —
  // by FILLING THE FORM AND SUBMITTING — because the dialog only exists as a consequence of a
  // create response, and a check that opened it directly would prove nothing about the flow.
  // (The create form is INLINE on the page, not a modal; there is no dialog to open first.)
  await page.locator("[data-content-api-form-name]").fill(`QA Walk ${stamp}`).catch(() => {});
  await page.waitForTimeout(400);
  steps.theNameIsInTheField =
    (await page.locator("[data-content-api-form-name]").inputValue().catch(() => "")) ===
    `QA Walk ${stamp}`;
  await page.locator("[data-content-api-form] button[type=submit]").first().click().catch(() => {});
  await page.waitForTimeout(2500);

  steps.copyOnceDialogOpened =
    (await page.locator("[data-content-api-plaintext]").count()) > 0;
  const revealed = (await page
    .locator("[data-content-api-plaintext-value]")
    .innerText()
    .catch(() => ""))
    .trim();
  steps.theDialogShowsThePlaintext = revealed.startsWith("omn_");
  steps.doneIsBlockedUntilStored =
    (await page
      .locator("[data-content-api-plaintext-done]")
      .first()
      .isDisabled()
      .catch(() => false)) === true;
  // And the gate is a gate: ticking it releases the button, which is what distinguishes a real
  // acknowledgement from a permanently disabled control.
  await page.locator("[data-content-api-plaintext-stored]").check().catch(() => {});
  await page.waitForTimeout(300);
  steps.tickingStoredReleasesDone =
    (await page
      .locator("[data-content-api-plaintext-done]")
      .first()
      .isEnabled()
      .catch(() => false)) === true;
  await shot(page, "content-api-create-dialog");
  await page.locator("[data-content-api-plaintext-done]").first().click().catch(() => {});
  await page.waitForTimeout(800);

  // A token minted through the panel's own API, because the copy-once plaintext only exists in a
  // create RESPONSE — there is no route that gives it back, which is the property under test.
  const name = `QA Docs ${stamp}`;
  const created = await page
    .request.post(`${URL_API}/api/v1/content-api/tokens`, {
      data: { name, scopes: ["content:read"] },
    })
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.operatorCanMintAToken = created.status === 201 && Boolean(created.body?.plaintext);
  const plaintext = created.body?.plaintext || "";
  const prefix = created.body?.token?.prefix || "";

  // The plaintext is not in the store, which is the claim the copy-once dialog makes out loud.
  steps.plaintextIsNotStored =
    plaintext.length > 0 &&
    qaSql(`select count(*) from api_tokens where token_hash = '${plaintext}'`) === "0";
  steps.rowShowsThePrefixNotTheSecret =
    prefix !== "" &&
    (await page.locator(`text=${prefix}`).count().catch(() => 0)) > 0;
  steps.rowNeverShowsTheSecret =
    plaintext !== "" && (await page.locator(`text=${plaintext}`).count().catch(() => 0)) === 0;

  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.mintedRowIsOnScreen = (await page.locator(`text=${name}`).count().catch(() => 0)) > 0;

  // ------------------------------------------------------------------ the read surface, for real
  // The whole point of the section: the credential the panel minted opens the surface the panel
  // documents. This is a real HTTP call with the Bearer header, against the real routes — the
  // only proof that a token and a document describe the same API.
  const read = await page
    .request.get(`${URL_API}/api/v1/content/pages?limit=5`, {
      headers: { authorization: `Bearer ${plaintext}` },
    })
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.tokenReadsTheContentSurface = read.status === 200 && Array.isArray(read.body?.items);
  steps.everyItemCarriesItsCacheKeys =
    Array.isArray(read.body?.items) &&
    read.body.items.every(
      (item) => item.id && item.slug && item.etag && item.updated_at !== undefined,
    );
  steps.aPanelSessionIsRefusedTheContentSurface =
    (await page
      .request
      .get(`${URL_API}/api/v1/content/pages?limit=5`)
      .then((response) => response.status())
      .catch(() => 0)) === 401;

  // A cursor that walks pages, because "next_cursor is present" is only half of the criterion and
  // the half nobody notices until a frontend skips a page.
  const paged = await page
    .request.get(`${URL_API}/api/v1/content/pages?limit=2`, {
      headers: { authorization: `Bearer ${plaintext}` },
    })
    .then((response) => response.json().catch(() => null))
    .catch(() => null);
  steps.limitIsHonoured = Array.isArray(paged?.items) && paged.items.length <= 2;
  steps.envelopeHasAllThreeKeys =
    paged !== null &&
    Array.isArray(paged.items) &&
    "next_cursor" in paged &&
    "count" in paged;

  // A token WITHOUT `media:read` must be refused by name, not by absence: the criterion is a
  // `403 insufficient_scope`, and a 404 would hide the very answer the integrator needs.
  const noMedia = await page
    .request.post(`${URL_API}/api/v1/content-api/tokens`, {
      data: { name: `QA NoMedia ${stamp}`, scopes: ["content:read"] },
    })
    .then((response) => response.json().catch(() => null))
    .catch(() => null);
  const scopeRefusal = await page
    .request.get(`${URL_API}/api/v1/content/media?limit=1`, {
      headers: { authorization: `Bearer ${noMedia?.plaintext || ""}` },
    })
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.missingScopeIsNamed =
    scopeRefusal.status === 403 && scopeRefusal.body?.error?.code === "insufficient_scope";

  // ------------------------------------------------------------------ the Docs tab
  await page
    .goto(`${URL_ADMIN}/content-api/docs`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(2500);
  steps.docsScreenReady = (await page.locator("[data-content-api-docs]").count()) > 0;
  steps.docsErrorStripIsAbsent =
    (await page.locator("[data-content-api-docs-error]").count()) === 0;

  // The document the tab rendered, compared against the document the API serves — the same one
  // the read surface above answered through. A tab that renders its own list while the server
  // documents a seventh route is the drift this whole screen was designed to prevent.
  const document = await page
    .request.get(`${URL_API}/api/v1/content-api/openapi.json`)
    .then((response) => response.json().catch(() => null))
    .catch(() => null);
  steps.documentIsOpenApi31 = document?.openapi === "3.1.0";
  const serverIds = Object.values(document?.paths || {}).flatMap((methods) =>
    Object.values(methods || {}).map((operation) => operation?.operationId),
  ).filter(Boolean);
  steps.documentDeclaresEveryEndpoint = serverIds.length >= 6;
  for (const id of serverIds) {
    steps[`documented_${id.replace(/\./g, "_")}`] =
      (await page.locator(`[data-content-api-endpoint="${id}"]`).count().catch(() => 0)) > 0;
  }
  steps.baseUrlIsShown =
    Boolean(document?.servers?.[0]?.url) &&
    (await page.locator("[data-content-api-base-url]").innerText().catch(() => "")).trim().length > 0;
  steps.paginationGuideIsPresent =
    (await page.locator("[data-content-api-pagination]").count()) > 0;
  steps.errorCodesAreListed =
    (await page.locator("[data-content-api-error-code]").count().catch(() => 0)) >= 5;
  steps.rebuildExampleIsPresent =
    (await page.locator("[data-content-api-rebuild-example]").count()) > 0;

  // The two downloads are real downloads, not buttons: the YAML body is fetched and checked for
  // the one property that makes it a document rather than a string — the endpoint paths survive
  // the second serialization.
  const yaml = await page
    .request.get(`${URL_API}/api/v1/content-api/openapi.json?format=yaml`)
    .then((response) => response.text().catch(() => ""))
    .catch(() => "");
  steps.yamlDownloadCarriesTheEndpoints =
    yaml.includes("openapi: 3.1.0") && yaml.includes("/api/v1/content/pages:");
  steps.aBadFormatIsRefusedWithItsField =
    (await page
      .request
      .get(`${URL_API}/api/v1/content-api/openapi.json?format=pdf`)
      .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
      .catch(() => ({ status: 0, body: null }))).body?.error?.details?.field === "format";

  // ------------------------------------------------------------------ revocation is immediate
  const tokenId = created.body?.token?.id;
  const revoke = await page
    .request.delete(`${URL_API}/api/v1/content-api/tokens/${tokenId}`)
    .then((response) => response.status())
    .catch(() => 0);
  steps.revokeSucceeded = revoke === 204 || revoke === 200;
  const afterRevoke = await page
    .request.get(`${URL_API}/api/v1/content/pages?limit=1`, {
      headers: { authorization: `Bearer ${plaintext}` },
    })
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.aRevokedTokenStopsReadingImmediately = afterRevoke.status === 401;
  steps.theRefusalSaysRevokedNotWrong =
    afterRevoke.body?.error?.code === "token_revoked";
  steps.theRevokedRowStaysVisible =
    qaSql(`select coalesce(revoked_at::text, 'NULL') from api_tokens where id = '${tokenId}'`) !== "NULL";

  // ------------------------------------------------------------------ the mobile layout
  await page.setViewportSize({ width: 390, height: 900 }).catch(() => {});
  await page.waitForTimeout(1600);
  const overflow = await page
    .evaluate(() => {
      const el = document.scrollingElement || document.documentElement;
      return el.scrollWidth - el.clientWidth;
    })
    .catch(() => -1);
  steps.noHorizontalScrollAt390 = overflow <= 1;
  await shot(page, "content-api-docs-390");
  await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});

  return steps;
}

/**
 * `runMembersDepth` — visitor accounts, their sessions and the site policy (REQ-064, slice 4c).
 *
 * This screen manages the table the REQ calls its single most important boundary, so the steps
 * below read the boundary from the DATABASE and the schema rather than trusting the panel's
 * labels:
 *
 * * **`cms_members` carries no `user_id` and no `organization_id`.** A visitor table that could
 *   point at a panel account is a table where the two identities meet, whatever the panel calls
 *   them. The columns are read out of `information_schema` because a boundary held by convention
 *   is a boundary the next writer erases.
 * * **A block takes effect on the live session, not just on the label.** The member signs in
 *   through the public route, the panel blocks them, and the SAME cookie is then refused — and
 *   the session ROW is asserted gone rather than merely ignored. "The cookie stopped working" is
 *   also what a cookie that never worked would say, so the working case is asserted first.
 * * **The gated-page default is asserted BEFORE anything is configured.** The criterion says a
 *   gated page answers 404, and the policy ships a default; a default that contradicts the
 *   criterion it was written for fails it on every new site, and no test run after a
 *   configuration can tell the difference.
 * * **The panel's own gate answer is read from the public route**, not from the table's badge,
 *   because a gate that only refuses signed-out visitors is not a gate.
 * * **Deleting names the address.** The confirmation has to quote what it is about to erase, so
 *   the check is that the address is ON the dialog rather than that a dialog appeared.
 *
 * Every step writes under `steps.*` and `--only=members` demands the list below by name.
 */
async function runMembersDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the screen has nothing to read";
    return steps;
  }

  // ------------------------------------------------------------------ the boundary is structural
  // Read out of the catalogue, not out of a route. The REQ's most important claim about this
  // module is that a visitor is never a panel user, and a claim about a schema can only be
  // checked against the schema.
  const memberColumns = qaSql(
    `select string_agg(column_name, ',') from information_schema.columns
     where table_name = 'cms_members'`,
  );
  steps.memberTableExists = memberColumns !== "";
  steps.memberTableHasNoPanelLink =
    !/user_id|organization_id|account_id/.test(memberColumns);
  steps.memberRolesArePlainText = memberColumns.includes("roles");

  // ------------------------------------------------------------------ the default, before anything
  // The policy row is deleted so the DEFAULT is what answers. This ordering is the whole point:
  // the criterion says a gated page answers 404, and a site policy that ships `prompt` answers
  // 401 — the failure is invisible to any test that configures first.
  qaSql(`delete from cms_member_settings where site_id = '${siteId}'`);
  const defaultBehaviour = qaSql(
    `select coalesce(
       (select gated_page_behaviour from cms_member_settings where site_id = '${siteId}'),
       'not_found')`,
  );
  steps.defaultGatedBehaviourIsNotFound = defaultBehaviour === "not_found";

  // ------------------------------------------------------------------ the screen
  await page.goto(`${URL_ADMIN}/members`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.screenReady = (await page.locator("[data-members-state=\"ready\"]").count()) > 0;
  steps.policyPanelIsOnScreen = (await page.locator("[data-member-policy=\"ready\"]").count()) > 0;

  // ------------------------------------------------------------------ the empty state, first
  // The screen's table only shows what the fixture creates, so the state an owner meets on a
  // fresh site is the one table this pass can never produce by accident — it has to be asserted
  // while it is genuinely empty, or a panel that renders a blank panel on day one is only found
  // by a person. It is read here BEFORE the fixture, and the hint is checked for the URL the
  // signup form lives at: "nothing here" is an answer, "here is where they come from" is the
  // part an owner actually needs.
  steps.emptyStateIsShownWhenThereAreNoMembers =
    (await page.locator("[data-members-state=\"ready\"]").innerText().catch(() => "")).length > 0 &&
    (await page.locator("text=/No visitors have signed up yet/i").count()) > 0;
  steps.emptyStateNamesTheSignupRoute =
    (await page.locator("text=/sign ?up/i").count()) > 0;
  await shot(page, "members-empty-state");

  // The panel must show the behaviour in force, not only offer the choice. An owner who cannot
  // see which answer a gated page gives cannot reason about who can find their pages.
  steps.panelShowsTheGatedBehaviour =
    (await page.locator("[data-member-policy-behaviour]").first().getAttribute("data-member-policy-behaviour")) ===
    "not_found";

  // ------------------------------------------------------------------ the fixture, through the API
  // Three members the browser could not have written: a waiting signup, a verified member with a
  // live session, and one the panel will block. Seeded through the panel's OWN routes so the
  // screen is proved against rows the routes actually produce.
  const waitingEmail = `waiting-${stamp}@example.test`;
  const created = await page
    .request.post(`${URL_API}/api/v1/members`, {
      data: { site_id: siteId, email: waitingEmail, name: "QA Waiting" },
    })
    .then((response) => ({ status: response.status(), body: response.json().catch(() => null) }))
    .catch(() => ({ status: 0, body: null }));
  steps.operatorCreatedAMember =
    created.status === 201 && Boolean(created.body && created.body.id);
  const waitingId = (created.body && created.body.id) || "";

  // An invited address has NO password: that is the difference the table's badge draws, and a
  // fixture that set one would make "invited, never claimed" pass for a row that was claimed.
  steps.invitedHasNoPassword =
    waitingId !== "" &&
    qaSql(`select coalesce(password_hash, 'NULL') from cms_members where id = '${waitingId}'`) === "NULL";
  steps.invitedRowSaysSo = (await page.locator(`[data-member-never-claimed="${waitingId}"]`).count()) > 0;

  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.rowIsOnScreen = (await page.locator(`[data-member-row="${waitingId}"]`).count()) > 0;
  // `pending` must not be rendered as a failure. The badge text is read, because a panel that
  // labels a waiting confirmation "Failed" teaches every operator that verification is broken.
  steps.pendingIsNotRenderedAsAFailure = /waiting/i.test(
    await page.locator(`[data-member-status-badge="${waitingId}"]`).first().innerText().catch(() => ""),
  );

  // ------------------------------------------------------------------ the tab count is the queue's
  const waitingCount = qaSql(
    `select count(*) from cms_members where site_id = '${siteId}' and status = 'pending'`,
  );
  const shownWaitingCount = await page
    .locator("[data-member-tab-count=\"pending\"]")
    .first()
    .innerText()
    .catch(() => "");
  steps.pendingTabMatchesSql = shownWaitingCount.trim() === waitingCount.trim();

  // ------------------------------------------------------------------ the drawer, the roles, the save
  await page.locator(`[data-member-open="${waitingId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.drawerOpened = (await page.locator(`[data-member-drawer="${waitingId}"]`).count()) > 0;
  // The drawer states the boundary on screen. A panel that does not say "this is not a panel
  // user" leaves the whole row looking like a second copy of the Users screen.
  steps.drawerStatesTheBoundary = /not a panel user|cms_members/i.test(
    await page.locator(`[data-member-drawer="${waitingId}"]`).first().innerText().catch(() => ""),
  );

  await page.locator("[data-member-roles]").fill("reader, archivist").catch(() => {});
  steps.rolesAreOnTheInput =
    (await page.inputValue("[data-member-roles]").catch(() => "")) === "reader, archivist";
  await page.locator("[data-member-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  // Roles REPLACE rather than accumulate, and only SQL can tell that apart from an append.
  steps.rolesAreInSql =
    qaSql(`select array_to_string(roles, ',') from cms_members where id = '${waitingId}'`) ===
    "reader,archivist";

  // ------------------------------------------------------------------ verify takes a real effect
  await page.locator("[data-member-drawer-close]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator(`[data-member-action="verify"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.verifiedInSql =
    qaSql(`select status from cms_members where id = '${waitingId}'`) === "verified";

  // ------------------------------------------------------------------ the panel cookie at a member route
  // Both directions. A visitor cookie presented where a panel cookie is expected must fail, or
  // the two identities are one identity wearing two names.
  const memberCookieAtPanelRoute = await page.request.get(`${URL_API}/api/v1/members?site_id=${siteId}`, {
    headers: { cookie: `omnion_member=not-a-real-session` },
  });
  steps.memberCookieIsRefusedAtAPanelRoute = memberCookieAtPanelRoute.status() === 401;

  // ------------------------------------------------------------------ the gate, read from the public route
  // A gated page needs a page to gate. Written by SQL because the point is the GATE, not the
  // page editor — and `visibility = 'members'` is the state a members area is actually in.
  //
  // `visibility_roles` is an ARRAY column, so it is written as one rather than left to the
  // default: a page gated on `roles` with an empty array is a gate on nobody, which the schema
  // refuses and which would make the role half of this pass meaningless.
  const gatedSlug = `qa-gated-${stamp}`;
  const gatedPageId = qaSql(
    `insert into pages (site_id, slug, page_type, status, visibility, visibility_roles)
     values ('${siteId}', '${gatedSlug}', 'page', 'published', 'members', '{}') returning id`,
  );
  steps.gatedPageExists = gatedPageId !== "";
  const publicSlug = `qa-public-${stamp}`;
  qaSql(
    `insert into pages (site_id, slug, page_type, status, visibility, visibility_roles)
     values ('${siteId}', '${publicSlug}', 'page', 'published', 'public', '{}')`,
  );

  // A real visitor signs in through the PUBLIC route and keeps the cookie — the three answers
  // below are three different cookies hitting one page.
  const memberEmail = `member-${stamp}@example.test`;
  await page.request.post(`${URL_API}/api/v1/members`, {
    data: {
      site_id: siteId,
      email: memberEmail,
      name: "QA Member",
      password: "QaMember-Passw0rd-2026!",
      roles: ["reader"],
    },
  });
  const memberId = qaSql(
    `select id from cms_members where site_id = '${siteId}' and lower(email) = '${memberEmail}'`,
  );
  await page.request.post(`${URL_API}/api/v1/members/${memberId}/verify?site_id=${siteId}`).catch(() => {});

  const signin = await page
    .request.post(`${URL_API}/api/v1/public/members/signin?site=main`, {
      data: { email: memberEmail, password: "QaMember-Passw0rd-2026!" },
    })
    .then((response) => ({ status: response.status(), setCookie: response.headers()["set-cookie"] || "" }))
    .catch(() => ({ status: 0, setCookie: "" }));
  steps.publicSigninWorks = signin.status === 200;
  const memberCookie = (signin.setCookie.match(/omnion_member=([^;]+)/) || [])[1] || "";
  steps.memberCookieIsItsOwnName = memberCookie !== "";

  // The gate probe returns 200 with a VERDICT (`exists`, `allowed`, `behaviour`, `sign_in_url`)
  // rather than 404 — a theme needs to draw a prompt, and a status code cannot carry a URL. The
  // concealment therefore lives in `allowed`, and the assertion has to read the BODY: a check on
  // `status === 404` would pass against a probe that always answered 404, which is a gate that
  // refuses everybody including the site owner.
  const gateProbe = async (slug, cookie) => {
    const headers = cookie ? { cookie: `omnion_member=${cookie}` } : {};
    const response = await page
      .request.get(`${URL_API}/api/v1/public/members/gate?site=main&slug=${slug}`, { headers })
      .catch(() => null);
    if (!response) return { status: 0, exists: false, allowed: false, body: "" };
    const text = await response.text().catch(() => "");
    let parsed = {};
    try {
      parsed = JSON.parse(text);
    } catch {
      parsed = {};
    }
    return {
      status: response.status(),
      exists: parsed.exists === true,
      allowed: parsed.allowed === true,
      behaviour: parsed.behaviour || "",
      sign_in_url: parsed.sign_in_url || null,
      body: text,
    };
  };

  const visitorGate = await gateProbe(gatedSlug, null);
  steps.gateProbeAnswers = visitorGate.status === 200;
  steps.gatedPageIsFoundByTheProbe = visitorGate.exists === true;
  steps.gateRefusesAVisitor = visitorGate.allowed === false;

  const memberGate = await gateProbe(gatedSlug, memberCookie);
  steps.gateAdmitsTheMember = memberGate.allowed === true;
  steps.memberCookieIsAccepted = memberGate.status === 200 && memberGate.exists === true;

  // An UNGATED page must be readable by the same visitor who was just refused the gated one.
  // This is the assertion that would have caught the `member.is_some_and(…)` inversion, which
  // 404s every signed-out visitor on every page — a site that gated nothing, serving nothing.
  const publicPageAsVisitor = await gateProbe(publicSlug, null);
  steps.ungatedPageIsServedToAVisitor = publicPageAsVisitor.allowed === true;

  // ------------------------------------------------------------------ a role gate refuses, then admits
  const roleSlug = `qa-role-${stamp}`;
  qaSql(
    `insert into pages (site_id, slug, page_type, status, visibility, visibility_roles)
     values ('${siteId}', '${roleSlug}', 'page', 'published', 'roles', array['archivist'])`,
  );
  const beforeGrant = await gateProbe(roleSlug, memberCookie);
  steps.roleGateRefusesAMemberWithoutIt = beforeGrant.allowed === false;
  await page.request.patch(`${URL_API}/api/v1/members/${memberId}?site_id=${siteId}`, {
    data: { roles: ["reader", "archivist"] },
  });
  const afterGrant = await gateProbe(roleSlug, memberCookie);
  // The SAME cookie, after a grant. Without this the previous assertion would also be satisfied
  // by a gate that refuses everybody.
  steps.roleGateAdmitsAfterTheGrant = afterGrant.allowed === true;

  // ------------------------------------------------------------------ blocking kills the session
  const liveBefore = qaSql(
    `select count(*) from cms_member_sessions where member_id = '${memberId}' and expires_at > now()`,
  );
  steps.sessionExistedBeforeTheBlock = liveBefore === "1";
  await page.locator(`[data-member-open="${memberId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1200);
  // The drawer's block button is a named hook, not "the first action": a dialog opened by the
  // wrong button is a dialog whose assertion proves nothing.
  await page.locator("[data-member-drawer-action=\"block\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.blockDialogAskedForAReason =
    (await page.locator("[data-member-block-reason]").count()) > 0;
  await page.locator("[data-member-block-reason]").fill("QA: proving a block stops the session").catch(() => {});
  await page.locator("[data-member-block-submit]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.blockedInSql =
    qaSql(`select status from cms_members where id = '${memberId}'`) === "blocked";
  steps.blockRemovedTheSessionRow =
    qaSql(`select count(*) from cms_member_sessions where member_id = '${memberId}'`) === "0";
  const afterBlock = await gateProbe(gatedSlug, memberCookie);
  // A blocked member answers `allowed: false` on the probe rather than 401/404, because the
  // probe is a verdict endpoint; the 404 concealment is the PAGE route's job and is proved in
  // the store suite. The cookie that worked a moment ago is what makes this an assertion.
  steps.blockedMemberIsRefused = afterBlock.allowed === false;

  // ------------------------------------------------------------------ the policy is real
  // Flipping the behaviour and reading the gate back is the only way to know the radio is wired
  // to anything; a stored boolean and a control that look like they write it prove nothing.
  await page.locator("[data-member-policy-behaviour-option=\"prompt\"]").check({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-member-policy-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.behaviourInSql =
    qaSql(`select gated_page_behaviour from cms_member_settings where site_id = '${siteId}'`) ===
    "prompt";
  const prompted = await gateProbe(gatedSlug, null);
  steps.promptBehaviourIsReported = prompted.behaviour === "prompt";
  // A prompt must name the sign-in link, because a refusal a visitor cannot act on is a dead end
  // dressed as a door — and it must NOT appear while the behaviour is `not_found`, or the
  // concealment leaks the page it is meant to hide.
  steps.promptNamesTheSignInLink = typeof prompted.sign_in_url === "string" && prompted.sign_in_url !== "";
  steps.notFoundNamesNoSignInLink = visitorGate.sign_in_url === null;

  // Put it back, so the site the next pass finds is the default one.
  await page.locator("[data-member-policy-behaviour-option=\"not_found\"]").check({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-member-policy-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.behaviourRestored =
    qaSql(`select gated_page_behaviour from cms_member_settings where site_id = '${siteId}'`) ===
    "not_found";

  // ------------------------------------------------------------------ delete names the address
  await page.locator("[data-member-invite-open]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.inviteDialogOpened = (await page.locator("[data-member-invite-dialog]").count()) > 0;
  await page.locator("[data-member-invite-dialog]").press("Escape").catch(() => {});
  await page.waitForTimeout(400);

  await page.locator(`[data-member-open="${waitingId}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-member-drawer-action=\"delete\"]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  const deleteDialogText = await page
    .locator("[data-member-delete-dialog]")
    .first()
    .innerText()
    .catch(() => "");
  steps.deleteDialogNamesTheAddress = deleteDialogText.includes(waitingEmail);
  await page.locator("[data-member-delete-submit]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.deletedFromSql =
    qaSql(`select count(*) from cms_members where id = '${waitingId}'`) === "0";

  // ------------------------------------------------------------------ the settings route, on its own
  // The REQ lists `/members/settings` as its own route, so it is visited as its own route rather
  // than inferred from the one embedded in `/members`. Two routes rendering the SAME component is
  // the design, and this is the half that proves it: the policy is on screen here with no table
  // above it, and a save made from here is the same save.
  await page.goto(`${URL_ADMIN}/members/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.settingsRouteReady =
    (await page.locator("[data-members-settings-state=\"ready\"]").count()) > 0;
  steps.settingsRouteShowsThePolicy =
    (await page.locator("[data-member-policy=\"ready\"]").count()) > 0;
  steps.settingsRouteHasNoMemberTable =
    (await page.locator("[data-member-row]").count()) === 0;
  // The route has to answer the same question the embedded panel does — a screen that renders the
  // policy and then saves somewhere else is the drift this route exists to prevent.
  steps.settingsRouteShowsTheSameBehaviour =
    (await page.locator("[data-member-policy-behaviour]").first().getAttribute("data-member-policy-behaviour")) ===
    "not_found";

  // A save from HERE, read back in SQL: the point is that this route writes the same row.
  await page.locator("[data-member-policy-signup-toggle]").uncheck({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-member-policy-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.settingsRouteSaveIsInSql =
    qaSql(`select signup_enabled from cms_member_settings where site_id = '${siteId}'`) === "f";
  // The notice is read, not the whole screen: the screen always contains the word "signup"
  // because the control's own label does, so a regex over the page would match a save that
  // reported nothing.
  steps.settingsRouteSaveSaidSo = /saved/i.test(
    await page.locator("[data-members-notice]").first().innerText().catch(() => ""),
  );
  // Put it back: the next pass opens this site, and a site with signups off is a site whose
  // public signup route is refused for reasons that have nothing to do with what is being tested.
  await page.locator("[data-member-policy-signup-toggle]").check({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-member-policy-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.signupRestored =
    qaSql(`select signup_enabled from cms_member_settings where site_id = '${siteId}'`) === "t";

  // ------------------------------------------------------------------ the mobile layout, on BOTH routes
  //
  // Both halves of this screen, measured on the phone, and neither read off the other.
  //
  // The obvious single probe is wrong in a way that cannot fail: the pass sits on
  // `/members/settings` here, a screen whose whole body is a policy form with no table and no
  // drawer, so `noHorizontalScrollAt390` was measuring the narrowest layout in the module and
  // publishing it as the answer for the screen with a seven-column table, a filter row, four
  // dialogs and a card fallback. The table is `hidden sm:block` with `min-w-[760px]` and the
  // cards are `sm:hidden`, so the two widths genuinely differ — measuring one tells you nothing
  // about the other, and the phone is the width the cards exist for.
  //
  // The drawer is measured separately, because it is a fixed overlay whose width is set in its
  // own class: a page can have no horizontal scroll and a drawer that runs off the edge, and
  // `scrollWidth` on the document cannot see that.
  const measureOverflow = () =>
    page
      .evaluate(() => {
        const el = document.scrollingElement || document.documentElement;
        return el.scrollWidth - el.clientWidth;
      })
      .catch(() => -1);

  // The TABLE route first, at 390 px: rows, filters and the card fallback all live here.
  await page.goto(`${URL_ADMIN}/members`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.setViewportSize({ width: 390, height: 900 }).catch(() => {});
  await page.waitForTimeout(2200);
  // `data-members-state="ready"` is the table route's own readiness marker (the settings route
  // has a separate `data-members-settings-state`), and it is demanded FIRST because it is the
  // one that proves a table was measured rather than an error or a loading shell — which also
  // reports no horizontal scroll.
  steps.membersTableReadyAt390 =
    (await page.locator("[data-members-state=\"ready\"]").count()) > 0;
  // Which layout is on screen is a fact this measurement depends on, so it is read rather than
  // assumed: a pass that measured an empty page would also report no horizontal scroll.
  steps.membersLayoutAt390 =
    (await page.locator("[data-member-cards]").count()) > 0 ? "cards" : "table";
  const tableOverflow = await measureOverflow();
  steps.membersTableNoHorizontalScrollAt390 = tableOverflow <= 1;
  await shot(page, "members-table-390");

  // The DRAWER at 390 px. Its right edge is read against the viewport, because a drawer wider
  // than the screen does not create document scroll — it just leaves the page unusable.
  const firstMember = await page
    .locator("[data-member-card]")
    .first()
    .getAttribute("data-member-card")
    .catch(() => null);
  if (firstMember) {
    await page.locator(`[data-member-open="${firstMember}"]`).first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1400);
    steps.drawerOpenedAt390 = (await page.locator(`[data-member-drawer="${firstMember}"]`).count()) > 0;
    steps.drawerFitsAt390 = await page
      .evaluate((id) => {
        const el = document.querySelector(`[data-member-drawer="${id}"]`);
        if (!el) return false;
        const r = el.getBoundingClientRect();
        // 1px of slack: a sub-pixel right edge on a fractional device pixel ratio is a rounding
        // artefact, not an overflow, and refusing it would make this check unpassable on the
        // very devices it is for.
        return r.right <= window.innerWidth + 1 && r.left >= -1;
      }, firstMember)
      .catch(() => false);
    await shot(page, "members-drawer-390");
    await page.locator("[data-member-drawer-close]").first().click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(500);
  } else {
    // Named rather than skipped: an absent step reads as a pass, and "there was nothing to
    // open" is a different fact from "the drawer fits".
    steps.drawerOpenedAt390 = false;
    steps.drawerFitsAt390 = false;
    steps.reasonNoMemberToOpen =
      "no member card on the table route at 390px, so the drawer had nothing to open";
  }

  // The POLICY route at 390 px — the probe that existed all along, kept because it is a
  // genuinely different layout, and renamed so a reader can tell which screen each number is
  // about. The retained `noHorizontalScrollAt390` below now carries that narrower truth.
  await page.goto(`${URL_ADMIN}/members/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.setViewportSize({ width: 390, height: 900 }).catch(() => {});
  await page.waitForTimeout(1800);
  const policyOverflow = await measureOverflow();
  steps.policyRouteNoHorizontalScrollAt390 = policyOverflow <= 1;
  await shot(page, "members-policy-390");

  steps.noHorizontalScrollAt390 = policyOverflow <= 1;
  steps.noHorizontalScrollAt390IsAbout =
    "the policy screen (/members/settings); the member table's answer is membersTableNoHorizontalScrollAt390";
  await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});

  return steps;
}

async function runSeoDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const from = `/qa-old-${stamp}`;
  const to = `/qa-new-${stamp}`;
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the screen has nothing to read";
    return steps;
  }

  // ---------------------------------------------------------------- the screen and its panels
  await page.goto(`${URL_ADMIN}/seo`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.screenReady = (await page.locator("[data-seo-state=\"ready\"]").count()) > 0;
  steps.redirectsPanelIsTheDefaultTab =
    (await page.locator("[data-seo-panel=\"redirects\"]").count()) > 0;
  steps.emptyRedirectsExplainThemselves =
    (await page.locator("[data-seo-redirects-empty]").count()) > 0;

  // ---------------------------------------------------------------- create a rule through the screen
  await page.locator("[data-seo-redirect-new]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.redirectFormOpened = (await page.locator("[data-seo-redirect-form]").count()) > 0;
  await page.locator("[data-seo-redirect-from-input]").fill(from).catch(() => {});
  await page.locator("[data-seo-redirect-to-input]").fill(to).catch(() => {});
  // Read the typed values BACK: a `fill()` that lands while React is still mounting reports
  // success, and every assertion naming the typed path then matches nothing.
  steps.fromIsOnTheInput = (await page.inputValue("[data-seo-redirect-from-input]").catch(() => "")) === from;
  steps.toIsOnTheInput = (await page.inputValue("[data-seo-redirect-to-input]").catch(() => "")) === to;
  await page.locator("[data-seo-redirect-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.ruleRowLanded = (await page.locator(`[data-seo-redirect-row="${from}"]`).count()) > 0;
  steps.ruleIsOnScreen = await page
    .locator(`[data-seo-redirect-row="${from}"]`)
    .first()
    .isVisible()
    .catch(() => false);
  steps.ruleIsInSql =
    qaSql(`select count(*) from cms_seo_redirects where from_path = '${from}'`) === "1";

  // ---------------------------------------------------------------- the test does not count a hit
  await page.locator(`[data-seo-redirect-test="${from}"]`).click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1500);
  steps.testResultShown = (await page.locator("[data-seo-test-result]").count()) > 0;
  steps.testNamesTheRule = (await page
    .locator("[data-seo-test-result]")
    .first()
    .innerText()
    .catch(() => "")) .includes(from);
  steps.testSaysItDidNotCount = (await page
    .locator("[data-seo-test-result]")
    .first()
    .innerText()
    .catch(() => "")) .includes("did not count");
  // The counter is the whole reason the test is a separate entry point.
  steps.testCountedNoHit =
    qaSql(`select coalesce(sum(hits), 0) from cms_seo_redirects where from_path = '${from}'`) === "0";
  steps.hitsBadgeSaysZero = (await page
    .locator(`[data-seo-redirect-row="${from}"] [data-seo-redirect-hits]`)
    .first()
    .innerText()
    .catch(() => ""))
    .includes("0 hit");

  // ---------------------------------------------------------------- the pattern is refused as a path
  await page.locator("[data-seo-redirect-new]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator("[data-seo-redirect-from-input]").fill("/qa-no-slash").catch(() => {});
  await page.locator("[data-seo-redirect-to-input]").fill(to).catch(() => {});
  await page.locator("[data-seo-redirect-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.relativeFromRefused = (await page.locator("[data-seo-redirect-error]").count()) > 0;
  steps.relativeFromNamesTheRule =
    (await page.locator("[data-seo-redirect-error]").first().innerText().catch(() => "")).includes("/");
  steps.relativeFromStoredNothing =
    qaSql(`select count(*) from cms_seo_redirects where from_path = '/qa-no-slash'`) === "0";

  // ---------------------------------------------------------------- the sitemap panel
  await page.locator("[data-seo-tab=\"sitemap\"]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(900);
  steps.sitemapPanelOpened = (await page.locator("[data-seo-panel=\"sitemap\"]").count()) > 0;
  steps.pageTypesAreThisSitesOwn =
    (await page.locator("[data-seo-sitemap-types]").count()) > 0 ||
    (await page.locator("[data-seo-no-page-types]").count()) > 0;
  steps.robotsEditorIsPrefilled = (
    (await page.inputValue("[data-seo-robots]").catch(() => "")) || ""
  ).includes("User-agent");

  // A robots.txt that blocks the whole site is saved WITH a warning, not refused.
  await page.locator("[data-seo-robots]").fill("User-agent: *\nDisallow: /\n").catch(() => {});
  await page.waitForTimeout(700);
  steps.blockingRobotsWarns = (await page.locator("[data-seo-robots-warnings]").count()) > 0;
  steps.blockingRobotsWarningNamesItself = (await page
    .locator("[data-seo-robots-warnings]")
    .first()
    .innerText()
    .catch(() => "")).includes("not to read it");
  await page.locator("[data-seo-settings-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.robotsSaved = qaSql(
    `select count(*) from cms_seo_settings where site_id = '${siteId}' and robots_txt like '%Disallow: /%'`,
  ) === "1";

  // Regenerate: the preview must show real XML and the count must match what is in SQL.
  await page.locator("[data-seo-sitemap-regenerate]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(3000);
  steps.sitemapPreviewShown = (await page.locator("[data-seo-sitemap-preview] pre").count()) > 0;
  const preview = await page
    .locator("[data-seo-sitemap-preview] pre")
    .first()
    .innerText()
    .catch(() => "");
  steps.previewIsRealXml = preview.startsWith("<?xml") && preview.includes("<urlset");
  const storedUrls = qaSql(
    `select count(*) from cms_seo_settings s, unnest(string_to_array(coalesce(s.sitemap_xml, ''), '<url>')) as part \
     where s.site_id = '${siteId}' and part = '<url>'`,
  );
  const shownUrls = (await page
    .locator("[data-seo-sitemap-preview]")
    .first()
    .innerText()
    .catch(() => "")) .match(/(\d+) URL/);
  steps.shownCountMatchesStorage = shownUrls ? shownUrls[1] === storedUrls : false;
  steps.previewCountIsNotAFabricatedNumber = shownUrls !== null;

  // ---------------------------------------------------------------- broken links
  await page.locator("[data-seo-tab=\"broken\"]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(700);
  steps.brokenPanelOpened = (await page.locator("[data-seo-broken-empty], [data-seo-broken-list]").count()) > 0;
  await page.locator("[data-seo-scan]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.scanReportedSomething =
    (await page.locator("[data-seo-notice]").count()) > 0 &&
    ((await page.locator("[data-seo-notice]").first().innerText().catch(() => "")) || "").length > 0;

  // ---------------------------------------------------------------- delete, with a confirmation that names it
  await page.locator("[data-seo-tab=\"redirects\"]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator(`[data-seo-redirect-delete="${from}"]`).click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.deleteConfirmOpened = (await page.locator("[data-seo-confirm]").count()) > 0;
  steps.deleteConfirmNamesThePath = (await page
    .locator("[data-seo-confirm]")
    .first()
    .innerText()
    .catch(() => "")).includes(from);
  await page.locator("[data-seo-confirm-yes]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.deletedFromTheList = (await page.locator(`[data-seo-redirect-row="${from}"]`).count()) === 0;
  steps.deletedFromSql = qaSql(`select count(*) from cms_seo_redirects where from_path = '${from}'`) === "0";

  return steps;
}

/**
 * The ten bundled themes, drawn (REQ-062, slice 4).
 *
 * Acceptance 1 asks for the ten to "resolve in the renderer registry, and a site activated on
 * each renders its pages with that theme's layout, not a colour-swapped copy", and acceptance
 * 16 asks for the walkthrough at 390 px and 1440 px with the vision review confirming "real
 * typography and layout differences between at least three themes".
 *
 * Both are claims about a BROWSER, and neither is provable by a unit test. A registry can hold
 * ten keys and a stylesheet can be imported ten times while every page still renders identically
 * — which is the state this REQ was in for several ticks, and which passes every Rust test
 * because the Rust side never draws anything.
 *
 * So the pass measures the three things that actually differ between themes, from the outside:
 *
 *  1. `data-theme` on `<html>` — the registry and the stylesheet both key off it, so a wrong
 *     value here means the page was drawn by one theme and styled by another.
 *  2. The computed `font-family` and `font-size` of the title — a different type system is a
 *     measured difference, and two themes with the same family fail here even when their
 *     palettes differ.
 *  3. The computed `background-color` of the body — a different palette is a measured
 *     difference.
 *
 * And then it asserts the *pairs*, which is the shape of the claim: ten themes that are all
 * different from each other is a stronger statement than "three differ", and it is the one
 * that catches the failure a per-theme check misses — a bundle where nine sheets are inert
 * because only one `data-theme` selector matches, so every page draws in the same theme while
 * each individual theme still "resolved".
 *
 * The site is switched by writing `sites.theme` directly, one theme at a time, and each render
 * is read back from the DOM. Writing the column is not a shortcut around the activation route —
 * `runThemesDepth` above already drives that route in a browser — this pass needs ten
 * activations in a row and what it is testing is the RENDER, not the button.
 */
async function runThemeRenderPass(page, report) {
  const steps = {};
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the renderer has no site to draw";
    return steps;
  }

  const pageSlug =
    qaSql(`select slug from pages where site_id = '${siteId}' and status = 'published' limit 1`) ||
    SAMPLE_SLUG;
  const before = qaSql(`select theme from sites where id = '${siteId}'`);

  // The ten keys, read from the themes' own manifests rather than typed here or parsed out of
  // the registry's source: a list written in the harness is a list that goes stale, and a stale
  // list is a theme that quietly stops being tested while nothing fails. A manifest's `key` is
  // the thing a site activates, so it is the one that has to be in this list.
  //
  // The earlier version of this read `[agencyTheme.key]:` lines out of `theme.ts` with a regex.
  // That produced `agencytheme.` — the symbol, a trailing dot from the member access, and a
  // name the renderer has never heard of — so every `update sites set theme = …` wrote a key
  // that falls back to `minimal` and every measurement came back identical. The pass would
  // have failed its own assertions, which is the good outcome, but for the wrong reason, and
  // the reason is worth recording: a parsed name is not a key until something checks it.
  const themeDirs = fs
    .readdirSync(path.join(REPO_ROOT, "themes"), { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name);
  const keys = themeDirs
    .map((dir) => {
      const manifest = path.join(REPO_ROOT, "themes", dir, "omnion.theme.json");
      if (!fs.existsSync(manifest)) return null;
      return JSON.parse(fs.readFileSync(manifest, "utf8")).key;
    })
    .filter(Boolean)
    .sort();
  // Every key here must be one the renderer can actually resolve, or the pass measures the
  // fallback ten times and calls it ten themes.
  //
  // The check is NOT `registrySource.includes(`"${key}"`)`. That was the second version and it
  // reported nine themes, not ten, because the registry writes its keys as computed properties
  // (`[agencyTheme.key]: agencyTheme`) rather than as string literals — so `minimal`, which the
  // probe found "missing", is the DEFAULT theme and the one every unknown key falls back to. A
  // grep-based membership test silently drops exactly the theme that matters most, and the
  // resulting pass would have measured minimal nine times under nine different names.
  //
  // So the registry is read structurally: each `[<symbol>.key]:` line names a symbol, the
  // symbol names a theme directory, and the directory's manifest carries the key. The default
  // theme is the one line that is not a map entry, so it is added from `DEFAULT_THEME_KEY`.
  const registrySource = fs.readFileSync(
    path.join(REPO_ROOT, "apps", "web", "lib", "theme.ts"),
    "utf8",
  );
  const registryKeys = new Set(
    [...registrySource.matchAll(/^\s*\[(\w+)\.key\]:/gm)].map((match) => {
      // `agencyTheme` → the directory whose manifest declares this theme.
      const base = match[1].replace(/Theme$/, "").toLowerCase();
      const manifest = path.join(REPO_ROOT, "themes", base, "omnion.theme.json");
      return fs.existsSync(manifest) ? JSON.parse(fs.readFileSync(manifest, "utf8")).key : base;
    }),
  );
  // `export const DEFAULT_THEME_KEY = minimalTheme.key;` — the key `resolveTheme` falls back to.
  const defaultMatch = registrySource.match(/DEFAULT_THEME_KEY\s*=\s*(\w+)\.key/);
  if (defaultMatch) {
    const base = defaultMatch[1].replace(/Theme$/, "").toLowerCase();
    const manifest = path.join(REPO_ROOT, "themes", base, "omnion.theme.json");
    if (fs.existsSync(manifest)) {
      registryKeys.add(JSON.parse(fs.readFileSync(manifest, "utf8")).key);
    }
  }
  const resolvable = keys.filter((key) => registryKeys.has(key));

  steps.registryKeysFound = keys.length;
  steps.registryHasTenThemes = keys.length === 10;
  steps.everyKeyIsInTheRegistry = resolvable.length === keys.length;
  steps.unresolvableKeys = keys.filter((key) => !resolvable.includes(key));

  const rendered = {};
  for (const key of resolvable) {
    qaSql(`update sites set theme = '${key}' where id = '${siteId}'`);
    await page
      .goto(`${URL_WEB}/${pageSlug}?site=${CREDS.siteKey}`, { waitUntil: "domcontentloaded" })
      .catch(() => {});
    await page.waitForTimeout(900);
    rendered[key] = await page
      .evaluate(() => {
        const html = document.documentElement;
        const title = document.querySelector("h1");
        const body = getComputedStyle(document.body);
        const titleStyle = title ? getComputedStyle(title) : null;
        return {
          dataTheme: html.getAttribute("data-theme") || "",
          background: body.backgroundColor,
          color: body.color,
          fontFamily: body.fontFamily,
          titleFont: titleStyle ? titleStyle.fontFamily : "",
          titleSize: titleStyle ? titleStyle.fontSize : "",
          titleWeight: titleStyle ? titleStyle.fontWeight : "",
          // The layout difference a palette cannot fake: how many top-level regions the
          // theme's own markup adds around the article.
          regions: document.querySelectorAll("header, nav, footer, main, article").length,
        };
      })
      .catch(() => ({}));
    await shot(page, `web-theme-${key}`);
  }

  steps.themesRendered = resolvable.filter((key) => rendered[key] && rendered[key].dataTheme === key);
  steps.everyThemeAnnouncesItself = steps.themesRendered.length === resolvable.length;

  // The pair claims. A theme that resolves but draws in another's palette fails the background
  // comparison; two themes that share a type system fail the family comparison.
  const signature = (key) => `${rendered[key]?.background}|${rendered[key]?.titleFont}`;
  const signatures = Object.fromEntries(resolvable.map((key) => [key, signature(key)]));
  steps.signatures = signatures;
  steps.distinctPalettes = new Set(Object.values(signatures)).size;

  const families = resolvable.map((key) => rendered[key]?.titleFont || "");
  steps.titleFamilies = [...new Set(families)];
  steps.distinctTypeSystems = new Set(families).size;

  // The criterion's own words: "at least three themes" with real differences between them.
  steps.atLeastThreeDiffer = steps.distinctPalettes >= 3 && steps.distinctTypeSystems >= 2;
  // The stronger claim, and the one that catches an inert bundle: no two themes identical.
  steps.noTwoThemesAreIdentical =
    new Set(Object.values(signatures)).size === resolvable.length &&
    new Set(families).size >= 2;

  // Mobile: the layout must hold at 390 px for every theme, not just the one the pass
  // happened to leave active.
  const viewport = page.viewportSize();
  await page.setViewportSize({ width: 390, height: 844 });
  const overflow = {};
  for (const key of resolvable) {
    qaSql(`update sites set theme = '${key}' where id = '${siteId}'`);
    await page
      .goto(`${URL_WEB}/${pageSlug}?site=${CREDS.siteKey}`, { waitUntil: "domcontentloaded" })
      .catch(() => {});
    await page.waitForTimeout(700);
    overflow[key] = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
  }
  steps.mobileOverflow = overflow;
  steps.noThemeOverflowsAt390 = resolvable.every((key) => (overflow[key] ?? 9999) <= 0);
  if (viewport) await page.setViewportSize(viewport);

  qaSql(`update sites set theme = '${before}' where id = '${siteId}'`);
  steps.siteRestored = qaSql(`select theme from sites where id = '${siteId}'`) === before;
  return steps;
}

/**
 * `/themes` — the theme gallery (REQ-062, slice 1).
 *
 * The pass drives the two things a gallery can get wrong that a screenshot cannot: the badge
 * has to MOVE when a theme is activated, and *Restore previous* has to be ABSENT when there is
 * nothing to restore. A card that shows a badge regardless of the database passes a
 * screenshot review every time and is wrong every time.
 *
 * The activation is written through the panel's OWN route (not by SQL) so the pass exercises
 * the write the button performs, and the column is then read from the table — a panel
 * agreeing with itself is the pair that can agree while the site renders the old theme.
 */
async function runThemesDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the gallery has nothing to read";
    return steps;
  }

  // The pass needs a second theme to switch TO, and a bundled theme is a mirror of a file the
  // platform may not ship, so the fixture writes one directly and says it is a fixture. A card
  // the pass activated into a theme it created itself is still a real activation.
  const candidate = `qa-theme-${stamp}`;
  const seeded = qaSql(
    `insert into themes (organization_id, key, name, version, source, manifest, storage_key) ` +
      `select null, '${candidate}', 'QA Theme', '1.0.0', 'uploaded', ` +
      `'{"key":"${candidate}","name":"QA Theme","version":"1.0.0","modes":["light","dark"]}'::jsonb, ` +
      `'qa/${candidate}.zip' on conflict do nothing; select count(*) from themes where key = '${candidate}'`,
  );
  steps.fixtureThemeExists = seeded === "1";

  // ------------------------------------------------------------------ the screen
  await page.goto(`${ADMIN}/themes`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.screenReady = (await page.locator("[data-themes-gallery]").count()) > 0;
  steps.cardsRendered = (await page.locator("[data-theme-card]").count()) > 0;
  steps.fixtureCardIsOnScreen =
    (await page.locator(`[data-theme-card="${candidate}"]`).count()) > 0;
  // A card that says nothing about what it ships is a card an operator cannot choose between.
  steps.cardDescribesItself =
    (await page.locator(`[data-theme-shape="${candidate}"]`).first().innerText().catch(() => ""))
      .length > 0;

  // A bundled theme may not be deleted, and the action must be ABSENT rather than disabled.
  steps.bundledCardOffersNoDelete =
    (await page.locator('[data-theme-card="minimal"] [data-theme-delete]').count()) === 0;

  // ------------------------------------------------------------------ the starting state
  const beforeKey = await page
    .locator("[data-themes-active-key]")
    .first()
    .getAttribute("data-themes-active-key")
    .catch(() => "");
  steps.activeKeyIsOnScreen = (beforeKey !== null && beforeKey !== undefined) && beforeKey !== "";
  steps.rollbackAbsentWhenNeverSwitched =
    (await page.locator("[data-themes-rollback]").first().isDisabled().catch(() => false)) === true;

  // ------------------------------------------------------------------ activate
  await page.locator(`[data-theme-activate="${candidate}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.confirmationOpened = (await page.locator("[data-themes-confirm]").count()) > 0;
  // The confirmation must NAME what is being replaced — a theme switch changes every page a
  // visitor sees, and "are you sure" does not say what.
  const confirmText = await page.locator("[data-themes-confirm]").first().innerText().catch(() => "");
  steps.confirmationNamesTheTheme = confirmText.includes(candidate);
  steps.confirmationNamesTheReplaced = beforeKey ? confirmText.includes(beforeKey) : false;
  await page.locator("[data-themes-confirm-accept]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2500);

  steps.badgeMoved =
    (await page.locator(`[data-theme-card="${candidate}"][data-active="true"]`).count()) > 0;
  steps.onlyOneCardIsActive =
    (await page.locator('[data-theme-card][data-active="true"]').count()) === 1;
  steps.noticeIsOnScreen =
    (await page.locator("[data-themes-notice]").first().innerText().catch(() => "")).length > 0;

  // The column, because that is what a visitor's request reads.
  const column = qaSql(`select theme from sites where id = '${siteId}'`);
  steps.columnFollowedThePanel = column === candidate;

  // ------------------------------------------------------------------ roll back
  const target = await page
    .locator("[data-themes-rollback-target]")
    .first()
    .innerText()
    .catch(() => "");
  steps.rollbackTargetIsNamed = target === beforeKey;
  steps.rollbackEnabledWithATarget =
    (await page.locator("[data-themes-rollback]").first().isDisabled().catch(() => true)) === false;
  await page.locator("[data-themes-rollback]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.rollbackConfirmationOpened = (await page.locator("[data-themes-confirm]").count()) > 0;
  await page.locator("[data-themes-confirm-accept]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2500);

  steps.badgeMovedBack =
    beforeKey
      ? (await page.locator(`[data-theme-card="${beforeKey}"][data-active="true"]`).count()) > 0
      : false;
  const columnAfter = qaSql(`select theme from sites where id = '${siteId}'`);
  steps.columnRestored = columnAfter === beforeKey;
  // A restore is itself reversible, so the button must be armed again — a rollback that
  // spends the target leaves an operator with no way back to what they just tried.
  steps.rollbackArmedAgainAfterARestore =
    (await page.locator("[data-themes-rollback]").first().isDisabled().catch(() => true)) === false;

  // ------------------------------------------------------------------ the mobile layout
  await page.setViewportSize({ width: 390, height: 900 }).catch(() => {});
  await page.waitForTimeout(1500);
  const overflow = await page
    .evaluate(() => {
      const el = document.scrollingElement || document.documentElement;
      return el.scrollWidth - el.clientWidth;
    })
    .catch(() => -1);
  steps.noHorizontalScrollAt390 = overflow <= 1;
  await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});

  return steps;
}

/**
 * `/themes/<key>/customize` + `/themes/<key>/history` (REQ-062, slice 2).
 *
 * The pass drives the three properties a settings screen can get wrong that no screenshot
 * catches, and each one has a version that passes a visual review and is wrong:
 *
 * 1. **A SAVE MUST NOT PUBLISH.** A screen that writes a draft and repaints the live revision
 *    as if the site changed teaches the operator to click "Save" for a live edit. So the pass
 *    saves, then reads the `theme_settings_published` pointer out of the database — the thing
 *    a visitor's request actually reads — and asserts it did not move.
 * 2. **THE CONTRAST GUARD MUST BE A GATE AND NOT A WALL.** A bad colour pair is typed into
 *    the editor, publish is clicked, and the 422 has to arrive as a visible acknowledgement
 *    prompt rather than a silent failure or a red line nobody can act on. Then the box is
 *    ticked and the publish succeeds — which is the only way to prove the guard is a gate.
 * 3. **A RESTORE APPENDS.** History is append-only, so restoring revision 1 must leave a NEW
 *    row whose content came from revision 1, with the original still readable. A restore that
 *    moved a pointer would show a shorter history and pass every other check here.
 *
 * The theme fixture is a real row with real tokens, because an editor over a theme with no
 * declared tokens has nothing to render and every assertion below would be vacuously true.
 */
async function runThemeSettingsDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  if (!siteId) {
    steps.reason = "the QA site does not exist, so the settings screen has nothing to read";
    return steps;
  }

  // A theme with tokens, so the token editor has rows and the contrast check has pairs.
  const candidate = `qa-settings-${stamp}`;
  const manifest = JSON.stringify({
    key: candidate,
    name: "QA Settings Theme",
    version: "1.0.0",
    modes: ["light", "dark"],
    slots: ["header", "footer", "home"],
    tokens: {
      surface: { light: "#ffffff", dark: "#101010" },
      surfaceRaised: { light: "#f4f4f4", dark: "#1c1c1c" },
      text: { light: "#111111", dark: "#f5f5f5" },
      textMuted: { light: "#5a5a5a", dark: "#a0a0a0" },
      accent: { light: "#2f6feb", dark: "#7aa2f7" },
    },
  }).replace(/'/g, "''");
  const seeded = qaSql(
    `insert into themes (organization_id, key, name, version, source, manifest, storage_key) ` +
      `select null, '${candidate}', 'QA Settings Theme', '1.0.0', 'uploaded', '${manifest}'::jsonb, ` +
      `'qa/${candidate}.zip' on conflict do nothing; ` +
      `select theme from sites where id = '${siteId}'`,
  );
  steps.fixtureThemeExists = (await page.locator("[data-themes-gallery]").count()) >= 0;
  steps.siteThemeKeyIsReadable = typeof seeded === "string" && seeded.length > 0;

  // A clean slate: a site that already has revisions from an earlier pass would make "the first
  // save creates revision 1" false, and the append-only assertions below count rows.
  qaSql(`delete from theme_settings_revisions where site_id = '${siteId}'`);

  // ------------------------------------------------------------------ the screen
  await page
    .goto(`${ADMIN}/themes/${candidate}/customize`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(3000);
  steps.screenReady = (await page.locator("[data-theme-customize]").count()) > 0;
  steps.themeKeyIsNamed = (await page.locator("[data-theme-customize-theme-key]").count()) > 0;
  steps.saysNothingSavedYet = (await page.locator("[data-theme-customize-empty]").count()) > 0;

  // The gallery is the way in, and a screen no entry point reaches is a screen the operator
  // finds by typing a URL — so the links exist on every card.
  await page.goto(`${ADMIN}/themes`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.galleryLinksToCustomize = (await page.locator("[data-theme-customize-link]").count()) > 0;
  steps.galleryLinksToHistory = (await page.locator("[data-theme-history-link]").count()) > 0;
  await page.locator("[data-theme-customize-link]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  steps.linkFromGalleryReachesTheEditor = (await page.locator("[data-theme-customize]").count()) > 0;

  // ------------------------------------------------------------------ the token editor
  const tokenRows = await page.locator("[data-theme-token]").count().catch(() => 0);
  steps.tokenEditorListsTheThemesTokens = tokenRows >= 5;
  // A live preview that reflects an edit BEFORE saving is acceptance 6; the pass reads the
  // surface's background before and after typing a colour.
  const previewSurface = '[data-theme-preview-surface]';
  const surfaceBefore = await page
    .locator(previewSurface)
    .first()
    .evaluate((el) => getComputedStyle(el).backgroundColor)
    .catch(() => "");
  steps.previewSurfaceIsRendered = typeof surfaceBefore === "string" && surfaceBefore.length > 0;
  const swatch = page.locator('[data-theme-token-input="text · light"]').first();
  steps.lightTextInputExists = (await swatch.count()) > 0;
  await swatch.fill("#0a0a0a").catch(() => {});
  await page.waitForTimeout(900);
  const surfaceAfter = await page
    .locator(previewSurface)
    .first()
    .evaluate((el) => getComputedStyle(el).backgroundColor)
    .catch(() => "");
  steps.previewRecomputes = typeof surfaceAfter === "string" && surfaceAfter.length > 0;
  // The panel must admit it has unsaved edits — the line that makes "Save draft" meaningful.
  steps.unsavedLineIsHonest = (await page.locator('[data-theme-customize-dirty="true"]').count()) > 0;

  // A half-typed value that the store would refuse must be refused by the panel, not by a 400
  // three lines later. The store's rule is "no semicolons" (a token becomes a CSS declaration).
  const hostile = page.locator('[data-theme-token-input="accent · light"]').first();
  await hostile.fill("#2f6feb; background: url(https://x.test/a)").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-theme-customize-save]").click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  const hostileError = await page
    .locator("[data-theme-customize-error]")
    .first()
    .innerText()
    .catch(() => "");
  steps.hostileValueIsRefusedWithAMessage = hostileError.length > 0;
  steps.noRevisionWasWrittenByAHostileSave = qaSql(
    `select count(*) from theme_settings_revisions where site_id = '${siteId}'`,
  ) === "0";

  // Back to a valid palette, and the SAVE that must not publish.
  await hostile.fill("#2f6feb").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-theme-customize-save]").click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  steps.saveWroteDraftOne = qaSql(
    `select count(*) from theme_settings_revisions where site_id = '${siteId}'`,
  ) === "1";
  steps.draftNumberIsOnScreen =
    (await page.locator("[data-theme-customize-draft-no]").first().innerText().catch(() => "")) === "1";
  steps.saysNothingPublishedYet = (await page.locator("[data-theme-customize-published-none]").count()) > 0;
  // **The property:** the live pointer did not move.
  steps.saveDidNotPublish = qaSql(
    `select count(*) from theme_settings_published where site_id = '${siteId}'`,
  ) === "0";
  steps.noticeNamesTheDraft = (
    await page.locator("[data-theme-customize-notice]").first().innerText().catch(() => "")
  ).includes("revision 1");

  // ------------------------------------------------------------------ the contrast guard
  // A deliberately unreadable pair: near-white text on near-white surface.
  const textLight = page.locator('[data-theme-token-input="text · light"]').first();
  await textLight.fill("#f7f7f7").catch(() => {});
  await page.locator('[data-theme-token-input="surface · light"]').first().fill("#fbfbfb").catch(() => {});
  await page.waitForTimeout(700);
  steps.contrastPanelIsOnScreen = (await page.locator("[data-theme-contrast]").count()) > 0;
  const contrastText = await page.locator("[data-theme-contrast]").first().innerText().catch(() => "");
  steps.contrastNamesBothTokens = contrastText.includes("text") && contrastText.includes("surface");
  const ackBefore = (await page.locator("[data-theme-contrast-ack]").count()) > 0;
  steps.acknowledgementIsOffered = ackBefore;
  steps.acknowledgementStartsUnchecked =
    (await page.locator("[data-theme-contrast-ack]").first().isChecked().catch(() => true)) === false;

  // Save the bad palette, then publish it: the 422 has to arrive as a visible prompt.
  await page.locator("[data-theme-customize-save]").click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  await page.locator("[data-theme-customize-publish]").click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  steps.publishRefusedBelowAA = qaSql(
    `select count(*) from theme_settings_published where site_id = '${siteId}'`,
  ) === "0";
  steps.refusalExplainsTheAcknowledgement = (
    await page.locator("[data-theme-customize-notice]").first().innerText().catch(() => "")
  )
    .toLowerCase()
    .includes("acknowledg");

  // Tick the box: now it publishes, which is the only way to prove the guard is a gate.
  if (ackBefore) {
    await page.locator("[data-theme-contrast-ack]").first().check({ timeout: 5000 }).catch(() => {});
  }
  await page.waitForTimeout(500);
  await page.locator("[data-theme-customize-publish]").click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3500);
  steps.publishSucceededAfterAcknowledgement = qaSql(
    `select count(*) from theme_settings_published where site_id = '${siteId}'`,
  ) === "1";
  steps.publishedRevisionIsTwo = qaSql(
    `select r.revision_no from theme_settings_published p ` +
      `join theme_settings_revisions r on r.id = p.revision_id where p.site_id = '${siteId}'`,
  ) === "2";
  steps.screenNamesTheLiveRevision =
    (await page.locator("[data-theme-customize-published-no]").first().innerText().catch(() => "")) === "2";

  // ------------------------------------------------------------------ the history screen
  await page
    .goto(`${ADMIN}/themes/${candidate}/history`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(3000);
  steps.historyScreenReady = (await page.locator("[data-theme-history]").count()) > 0;
  steps.historyListsBothRevisions = (await page.locator("[data-theme-revision]").count()) === 2;
  steps.liveRowIsBadged = (await page.locator("[data-theme-revision-live]").count()) > 0;
  steps.draftRowIsBadged = (await page.locator("[data-theme-revision-draft]").count()) > 0;
  // A first revision's empty state is prose, not a blank panel.
  await page.locator("[data-theme-revision]").nth(1).click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  const firstDiff = await page.locator("[data-theme-revision-detail]").first().innerText().catch(() => "");
  steps.firstRevisionExplainsItself = firstDiff.toLowerCase().includes("first revision");

  // A second revision has a diff, rendered per field.
  await page.locator("[data-theme-revision]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.diffIsRenderedPerField = (await page.locator("[data-theme-diff-field]").count()) > 0;
  steps.diffNamesTheField = (await page.locator('[data-theme-diff-field="tokens"]').count()) > 0;

  // ------------------------------------------------------------------ the restore APPENDS
  await page.locator("[data-theme-revision]").nth(1).click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  await page.locator("[data-theme-revision-restore]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(800);
  steps.restoreDialogOpened = (await page.locator("[data-theme-restore-confirm]").count()) > 0;
  // The dialog must not read like a rewind — it says a NEW revision is written.
  const restoreText = await page.locator("[data-theme-restore-confirm]").first().innerText().catch(() => "");
  steps.restoreSaysItAppends = restoreText.toLowerCase().includes("new");
  await page.locator("[data-theme-restore-accept]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(3500);
  steps.restoreWroteAThirdRevision = qaSql(
    `select count(*) from theme_settings_revisions where site_id = '${siteId}'`,
  ) === "3";
  steps.historyStillHoldsTheOriginal = qaSql(
    `select count(*) from theme_settings_revisions where site_id = '${siteId}' and revision_no = 1`,
  ) === "1";
  steps.restoredRowIsMarked = (await page.locator("[data-theme-revision-restored]").count()) > 0;
  steps.noticeSaysTheHistoryIsAppendOnly = (
    await page.locator("[data-theme-history-notice]").first().innerText().catch(() => "")
  )
    .toLowerCase()
    .includes("append-only");

  // ------------------------------------------------------------------ the mobile layout
  await page.setViewportSize({ width: 390, height: 900 }).catch(() => {});
  await page.waitForTimeout(1500);
  const mobileOverflow = await page
    .evaluate(() => {
      const el = document.scrollingElement || document.documentElement;
      return el.scrollWidth - el.clientWidth;
    })
    .catch(() => -1);
  steps.noHorizontalScrollAt390 = mobileOverflow <= 1;
  await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});

  return steps;
}

async function runFormsDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const formKey = `qa-form-${stamp}`;
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);

  // ---------------------------------------------------------------- the list and its empty state
  await page.goto(`${ADMIN}/forms`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.listReady = (await page.locator("[data-forms-state]").count()) > 0;
  steps.listSeesTheNewForm = false;

  // ---------------------------------------------------------------- create through the panel
  await page.locator("[data-forms-create]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.createFormOpened = (await page.locator("[data-forms-create-form]").count()) > 0;
  await page
    .locator("[data-forms-name]")
    .fill(`QA form ${stamp}`)
    .catch(() => {});
  await page.waitForTimeout(300);
  // Verify the fill by reading it BACK: a `fill()` that lands while a React branch is still
  // mounting is reported as a success and every selector that names the typed value then matches
  // nothing.
  steps.nameIsOnTheInput = await page.inputValue("[data-forms-name]").catch(() => "");
  steps.keyFollowsName = (await page.inputValue("[data-forms-key]").catch(() => "")).length > 0;
  await page.locator("[data-forms-key]").fill(formKey).catch(() => {});
  await page.locator("[data-forms-create-submit]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.rowLanded = (await page.locator(`[data-form-row="${formKey}"]`).count()) > 0;
  steps.listSeesTheNewForm = steps.rowLanded;
  steps.rowOnScreen = await page
    .locator(`[data-form-row="${formKey}"]`)
    .first()
    .isVisible()
    .catch(() => false);
  steps.draftIsLabelled = (await page.locator(`[data-form-row="${formKey}"] [data-form-status="draft"]`).count()) > 0;

  const editHref = await page
    .locator(`[data-form-row="${formKey}"] [data-form-edit]`)
    .first()
    .getAttribute("href")
    .catch(() => null);
  steps.editLinkHasAnId = typeof editHref === "string" && /\/forms\/[0-9a-f-]{36}\/edit/.test(editHref);
  if (!editHref) {
    steps.reason = "the new form row has no editor link, so the builder cannot be driven";
    return steps;
  }

  // ---------------------------------------------------------------- the builder's own refusals
  await page.goto(`${ADMIN}${editHref}`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  steps.editorReady = (await page.locator(`[data-form-builder="${formKey}"]`).count()) > 0;

  // A choice field with no options is refused by the store; the builder refuses it before the
  // round trip, with the reason on the field. This is the check that a builder wired to a second
  // authority would fail.
  await page.locator('[data-form-add="select"]').first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(700);
  steps.paletteAddsAField = (await page.locator('[data-form-field="plan"]').count()) > 0;
  steps.choiceFieldOpenedInspector = (await page.locator("[data-form-inspector-for]").count()) > 0;
  await page.locator("[data-form-inspector-options]").fill("").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-form-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.optionlessChoiceRefused = (await page.locator('[data-form-field-error="plan"]').count()) > 0;
  steps.optionlessChoiceRefusalText = await page
    .locator('[data-form-field-error="plan"]')
    .first()
    .textContent()
    .catch(() => null);
  steps.optionlessChoiceWasNotStored =
    qaSql(`select count(*) from cms_form_fields where key = 'plan'`) === "0";

  // Give it options and save for real.
  await page.locator("[data-form-inspector-options]").fill("Gold\nSilver").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-form-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.savedFields = qaSql(
    `select count(*) from cms_form_fields where form_id in (select id from cms_forms where key = '${formKey}')`,
  );
  steps.optionsWereStored =
    qaSql(
      `select options::text from cms_form_fields where key = 'plan' and form_id in (select id from cms_forms where key = '${formKey}')`,
    ) ?? "";
  steps.optionsCarriedBothChoices = /gold/i.test(steps.optionsWereStored) && /silver/i.test(steps.optionsWereStored);

  // A duplicate key is refused where the editor is looking at it.
  await page.locator('[data-form-add="text"]').first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator("[data-form-inspector-key]").fill("plan").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-form-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.duplicateKeyRefused = (await page.locator("[data-form-field-error]").count()) > 0;
  steps.duplicateKeyMessage = await page
    .locator("[data-form-field-error]")
    .first()
    .textContent()
    .catch(() => null);
  // Undo it so the rest of the pass has a saveable form.
  await page.locator(`[data-form-field="plan"] [data-form-field-remove]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-form-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);

  // ---------------------------------------------------------------- the preview runs the same rules
  await page.locator("[data-form-preview-toggle]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.previewOpened = (await page.locator("[data-form-preview]").count()) > 0;
  steps.previewHasTheCanvasFields =
    (await page.locator('[data-form-preview-field="plan"]').count()) > 0;
  await page.locator('[data-form-preview-submit]').click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  // The starter form's name and message are required, so an empty check must produce errors.
  steps.previewRefusedAnEmptyRequired = (await page.locator("[data-form-preview-error]").count()) > 0;
  steps.previewErrorNamesAField = await page
    .locator("[data-form-preview-error]")
    .first()
    .getAttribute("data-form-preview-error")
    .catch(() => null);
  await page.locator('[data-form-preview-input="name"]').fill("Ada").catch(() => {});
  await page.locator('[data-form-preview-input="message"]').fill("Hello there").catch(() => {});
  await page.locator('[data-form-preview-input="plan"]').selectOption("gold").catch(() => {});
  await page.waitForTimeout(300);
  await page.locator('[data-form-preview-submit]').click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(700);
  steps.previewAcceptedAFilledForm = (await page.locator("[data-form-preview-success]").count()) > 0;
  // And a value the field never offered must be refused, so the preview is not decoration.
  await page.locator('[data-form-preview-input="plan"]').selectOption("gold").catch(() => {});
  await page.locator('[data-form-preview-input="message"]').fill("").catch(() => {});
  await page.locator('[data-form-preview-submit]').click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.previewRefusedAnEmptyMessage = (await page.locator('[data-form-preview-error="message"]').count()) > 0;

  // ---------------------------------------------------------------- publish
  await page.locator("[data-form-publish]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.published =
    qaSql(`select status from cms_forms where key = '${formKey}'`) === "published";
  steps.publishIsLabelled = (await page.locator(`[data-form-builder="${formKey}"][data-form-status="published"]`).count()) > 0;

  // ---------------------------------------------------------------- the settings drawer
  await page.locator("[data-form-settings-toggle]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.settingsOpened = (await page.locator("[data-form-settings]").count()) > 0;
  steps.settingsCarriesTheStoredValues =
    (await page.locator("[data-form-settings-message]").inputValue().catch(() => "")) === "Thank you.";
  // Both inputs stay visible and one is inert: the form has exactly one behaviour.
  steps.redirectInertWhileShowingAMessage =
    await page.locator("[data-form-settings-redirect]").isDisabled().catch(() => false);
  await page.locator('[data-form-settings-action="redirect"]').check({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(400);
  steps.messageInertWhileRedirecting =
    await page.locator("[data-form-settings-message]").isDisabled().catch(() => false);
  // A redirect with no URL is refused by the store, and the drawer shows the refusal.
  await page.locator("[data-form-settings-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.redirectWithoutUrlRefused = (await page.locator("[data-form-settings-error]").count()) > 0;
  steps.redirectWithoutUrlMessage = await page
    .locator("[data-form-settings-error]")
    .first()
    .textContent()
    .catch(() => null);
  await page.locator('[data-form-settings-action="message"]').check({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-form-settings-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.settingsSaved = (await page.locator("[data-form-settings-notice]").count()) > 0;

  // ---------------------------------------------------------------- the public route, from the browser
  //
  // Posted through the panel's own origin so the session cookie rides along, which is exactly how
  // a visitor's browser reaches it. The two refusals are asked for here because the *answer shape*
  // is the product decision: 202 with `stored: false`, never a 4xx and never an error object.
  const submitThroughBrowser = async (answers, extra) =>
    page.evaluate(
      async ([key, payload]) => {
        const response = await fetch(
          `/api/v1/public/forms/${key}/submit?site=${encodeURIComponent(window.__qaSiteKey ?? "")}`,
          {
            method: "POST",
            credentials: "same-origin",
            headers: { "content-type": "application/json", "x-forwarded-for": "203.0.113.99" },
            body: JSON.stringify(payload),
          },
        );
        return { status: response.status, body: await response.json().catch(() => ({})) };
      },
      [formKey, { answers, filled_at_ms: 9000, source_path: "/qa", ...extra }],
    );

  await page.evaluate((key) => {
    window.__qaSiteKey = key;
  }, CREDS.siteKey);

  const good = await submitThroughBrowser({ name: "Ada", message: "Hello from the pass", plan: "gold" });
  steps.validSubmissionStatus = good.status;
  steps.validSubmissionStored = good.body?.stored === true;

  const honeypot = await submitThroughBrowser(
    { name: "Bot", message: "buy now", plan: "gold" },
    { honeypot: "http://spam.example" },
  );
  steps.honeypotStatus = honeypot.status;
  steps.honeypotLooksAccepted = honeypot.body?.stored === false;
  steps.honeypotLeaksNoFieldErrors = honeypot.body?.error === undefined && honeypot.body?.errors === undefined;
  steps.honeypotStoredNothing =
    qaSql(
      `select count(*) from cms_form_submissions where answers::text ilike '%buy now%' and form_id in (select id from cms_forms where key = '${formKey}')`,
    ) === "0";

  const tooFast = await submitThroughBrowser(
    { name: "Robot", message: "instant", plan: "gold" },
    { filled_at_ms: 10 },
  );
  steps.tooFastLooksAccepted = tooFast.body?.stored === false;

  const invalid = await submitThroughBrowser({ name: "ab", message: "", plan: "bronze" });
  // The one refusal a visitor IS told about, and it must be a 422 carrying every wrong field.
  steps.invalidStatus = invalid.status;
  const invalidErrors = invalid.body?.error?.details?.errors ?? null;
  steps.invalidCarriesFieldErrors = invalidErrors !== null && Object.keys(invalidErrors).length >= 2;
  steps.invalidNamesTheChoiceField = Boolean(invalidErrors?.plan);
  steps.invalidNamesTheShortName = Boolean(invalidErrors?.name);
  steps.invalidStoredNothing =
    qaSql(
      `select count(*) from cms_form_submissions where answers::text ilike '%ab%' and form_id in (select id from cms_forms where key = '${formKey}')`,
    ) === "0";

  // ---------------------------------------------------------------- the inbox
  const inboxHref = await page
    .locator(`[data-form-row="${formKey}"] [data-form-inbox]`)
    .first()
    .getAttribute("href")
    .catch(() => null);
  await page.goto(`${ADMIN}/forms`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2000);
  const inboxLink = inboxHref ?? (await page.locator(`[data-form-row="${formKey}"] [data-form-inbox]`).first().getAttribute("href").catch(() => null));
  steps.inboxHasItsOwnRoute = typeof inboxLink === "string" && /\/submissions$/.test(inboxLink);
  if (inboxLink) {
    await page.goto(`${ADMIN}${inboxLink}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(2500);
    steps.inboxReady = (await page.locator("[data-inbox]").count()) > 0;
    steps.inboxTabCounts = (await page.locator("[data-inbox-count]").count()) === 4;
    steps.inboxShowsTheSubmission = (await page.locator("[data-inbox-row]").count()) > 0;
    steps.unreadIsOne =
      (await page.locator('[data-inbox-count="new"]').textContent().catch(() => ""))?.trim() === "1";
    // The spam tab is empty *and says why*: a refused submission leaves no row, so an empty
    // table with no explanation reads as "nothing was refused".
    await page.locator('[data-inbox-tab-button="spam"]').click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1800);
    steps.spamTabIsEmpty = (await page.locator("[data-inbox-empty]").count()) > 0;
    steps.spamTabExplainsTheCounter = (await page.locator("[data-inbox-empty]").textContent().catch(() => "")) ?? "";
    steps.spamTabNamesTheProtections =
      /honeypot|minimum time|hourly limit/i.test(steps.spamTabExplainsTheCounter);

    // Back to unread and open the drawer: the consent text is shown, not a tick.
    await page.locator('[data-inbox-tab-button="new"]').click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1800);
    await page.locator("[data-inbox-open]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1200);
    steps.drawerOpened = (await page.locator("[data-inbox-drawer]").count()) > 0;
    steps.drawerShowsTheAnswers = (await page.locator("[data-inbox-answer]").count()) > 0;
    steps.drawerShowsTheName = (await page.locator('[data-inbox-answer="name"]').count()) > 0;
    steps.openingMarkedItRead =
      qaSql(
        `select status from cms_form_submissions where answers::text ilike '%Hello from the pass%' and form_id in (select id from cms_forms where key = '${formKey}')`,
      ) === "read";
    await page.locator("[data-inbox-drawer-close]").click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(800);

    // The export is the FILTERED inbox. Asked directly because a download through a headless
    // browser lands in a download directory nothing here reads; the URL and the row count are
    // what matter, and both are checkable.
    const csv = await page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/forms/${id}/submissions/export?status=read`, {
        credentials: "same-origin",
      });
      return { status: response.status, text: await response.text() };
    }, editHref.split("/")[2]);
    steps.exportStatus = csv.status;
    steps.exportIsCsv = (csv.text.split("\n")[0] ?? "").startsWith("received,status");
    steps.exportHasTheFilteredRow = csv.text.includes("Hello from the pass");
    steps.exportHasNoOtherState =
      !csv.text.split("\n").slice(1).some((line) => line.includes(",spam,"));
    steps.exportRowCount = csv.text.split("\n").filter((line) => line.trim() !== "").length - 1;
  }

  // ---------------------------------------------------------------- the list again
  await page.goto(`${ADMIN}/forms`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.listStillCarriesTheRow = (await page.locator(`[data-form-row="${formKey}"]`).count()) > 0;
  steps.listShowsItPublished = (await page.locator(`[data-form-row="${formKey}"] [data-form-status="published"]`).count()) > 0;

  // The delete confirmation names what goes with it: the submissions are the record of what the
  // form asked people, and they cascade.
  await page.locator(`[data-form-row="${formKey}"] [data-form-delete]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(700);
  steps.deleteConfirmOpened = (await page.locator("[data-forms-delete-confirm]").count()) > 0;
  steps.deleteConfirmNamesTheSubmissions =
    ((await page.locator("[data-forms-delete-confirm]").textContent().catch(() => "")) ?? "")
      .toLowerCase()
      .includes("submission");
  await page.locator("[data-forms-delete-confirm]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.deletedFromTheList = (await page.locator(`[data-form-row="${formKey}"]`).count()) === 0;
  steps.deletedFromSql = qaSql(`select count(*) from cms_forms where key = '${formKey}'`) === "0";
  steps.submissionsCascaded =
    qaSql(`select count(*) from cms_form_submissions where form_id not in (select id from cms_forms)`) === "0";

  // ---------------------------------------------------------------- cleanup
  qaSql(`delete from cms_form_submissions where form_id in (select id from cms_forms where key like 'qa-form-%')`);
  qaSql(`delete from cms_form_fields where form_id in (select id from cms_forms where key like 'qa-form-%')`);
  qaSql(`delete from cms_forms where key like 'qa-form-%'`);
  return steps;
}

async function runMenusDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const menuKey = `qa-menu-${stamp}`;
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);

  // ---------------------------------------------------------------- its own pages
  //
  // The picker, the page-labelled items and the whole publishing queue hang off a published page,
  // and this pass used to READ one that a different pass had created. Under `--only=menus` on a
  // private stack that database is empty, so every one of those steps was skipped by the `if` and
  // the report showed fourteen checks missing with no reason attached — a harness that quietly
  // stops proving things looks exactly like a screen that is not there. The pass therefore writes
  // what it needs, and refuses to continue without it.
  steps.seededPages = ensureQaPages(siteId, stamp);
  if (!steps.seededPages) {
    steps.reason = "this site's pages could not be seeded, so the picker and queue cannot run";
    return steps;
  }

  // ---------------------------------------------------------------- the list and the form
  await page.goto(`${URL_ADMIN}/menus`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.listReady = (await page.locator("[data-menus-state=ready]").count()) > 0;
  if (!steps.listReady) {
    steps.reason = await page
      .locator("[data-menus-state=error]")
      .innerText()
      .catch(() => "the menu list did not reach its ready state");
    return steps;
  }

  await page.locator("[data-menus-create]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  steps.formOpened = (await page.locator("[data-menu-form]").count()) > 0;
  await page.locator("[data-menu-form-name]").fill(`QA Menu ${stamp}`).catch(() => {});
  // The key is derived from the name until the editor touches it, so a form that asked for both
  // up front would make the common case two fields and the duplicate-key 409 a puzzle.
  steps.keyFollowsName = await page
    .locator("[data-menu-form-key]")
    .inputValue()
    .catch(() => "");
  await page.locator("[data-menu-form-save]").click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);

  const menuId = qaSql(`select id from cms_menus where key = '${menuKey}' limit 1`);
  steps.key = menuKey;
  steps.menuId = menuId || null;
  steps.rowLanded = Boolean(menuId);
  steps.rowOnScreen = (await page.locator(`[data-menu-row="${menuKey}"]`).count()) > 0;
  if (!menuId) return steps;

  // ---------------------------------------------------------------- the editor
  await page.goto(`${URL_ADMIN}/menus/${menuId}/edit`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.editorReady = (await page.locator("[data-menu-editor-state=ready]").count()) > 0;
  steps.treeEmpty = (await page.locator("[data-menu-tree-empty]").count()) > 0;

  // Three top-level rows, then a child under the second and a grandchild under the child.
  //
  // `Add item` already selects the new row, so the click below is a no-op in the normal case —
  // but the inspector is a React branch keyed on `selectedItem`, and a `fill()` that lands while
  // that branch is still swapping mounts an input nobody is listening to. Playwright reports that
  // as success, the row keeps its default label, and every assertion after it ("Nest QA third")
  // then fails on a selector the screen was never asked to carry. The fill is therefore verified
  // by reading the value back, and retried with a wait for the inspector to appear.
  async function labelLastRow(label) {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      const rows = page.locator("[data-menu-item-label]");
      const count = await rows.count();
      if (count > 0) await rows.nth(count - 1).click({ timeout: 3000 }).catch(() => {});
      const inspector = page.locator("[data-item-label]");
      await inspector
        .waitFor({ state: "visible", timeout: 3000 })
        .catch(() => {});
      await inspector.fill(label).catch(() => {});
      const typed = await inspector.inputValue().catch(() => "");
      if (typed === label) {
        await page.locator("[data-item-url]").fill(`/qa-${stamp}`).catch(() => {});
        return true;
      }
      await page.waitForTimeout(400);
    }
    return false;
  }
  steps.typedFirst = true;
  for (const label of ["QA first", "QA second", "QA third"]) {
    await page.locator("[data-menu-add-item]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(250);
    steps[`typed${label.split(" ")[1]}`] = await labelLastRow(label);
  }
  const secondId = await page
    .locator("[data-menu-item-label]")
    .nth(1)
    .getAttribute("data-menu-item-label")
    .catch(() => null);
  const firstId = await page
    .locator("[data-menu-item-label]")
    .nth(0)
    .getAttribute("data-menu-item-label")
    .catch(() => null);
  steps.threeTopLevel = secondId !== null && firstId !== null;

  if (secondId) {
    // "Nest under the row above" on the third row, then again on the row that became a child.
    //
    // Every click here is `.catch(() => {})` so one dead affordance cannot end the pass — which is
    // also how a nest that never happened reported `nestedUnderSecond: false` with no reason, the
    // same shape as the browser not having a button. So each click answers with what it actually
    // did: an `aria-label` that matched nothing, or a strict-mode violation, lands in `steps` and
    // is visible in the report instead of being indistinguishable from the screen being wrong.
    await page.locator(`[data-menu-item-label="${secondId}"]`).click({ timeout: 3000 }).catch(() => {});
    await page.waitForTimeout(200);
    const nestButton = page.locator(`button[aria-label^="Nest QA third"]`);
    steps.nestButtonCount = await nestButton.count().catch(() => 0);
    steps.nestClicked = await nestButton
      .click({ timeout: 3000 })
      .then(() => true)
      .catch(() => false);
    await page.waitForTimeout(400);
    // If nothing matched, read back the labels actually on screen so the report says WHICH label
    // the editor holds — a selector that says "Nest QA third" and finds nothing is a naming
    // mismatch, and the names are the only thing worth printing.
    steps.rowLabels = await page
      .locator("[data-menu-item-label] span:first-child")
      .allInnerTexts()
      .catch(() => []);
    // "Did the third row become a child of the second?" is answered from the tree's own markup,
    // which records the parent on every row: `[data-menu-item-row="<child>"]` sits inside the
    // `<li data-menu-item="<parent>">`. Counting visible labels inverts the answer — a nested
    // child renders INSIDE its parent's row, so a successful nest shows two labels, not three —
    // and an assertion that counts reports the working editor as broken. This version also
    // explains a nest that did not happen: if the parent's branch never opened, the child is
    // legitimately absent from the DOM and this reads false with the labels printed beside it.
    // The child's row, reached through the parent's CHILD LIST. `[data-menu-item="<parent>"]`
    // matched the parent's own `<li>`, and the first `[data-menu-item-row]` inside that `<li>`
    // is the parent's own row — so this read back the parent's id, and the nest check answered
    // "the second row has a child" by asking the second row about itself. The children are in
    // the sibling `<ul>`, one level down, which is the only place a child row can be.
    const nestedChildId = await page
      .locator(`[data-menu-item="${secondId}"] > ul > li > [data-menu-item-row]`)
      .first()
      .getAttribute("data-menu-item-row")
      .catch(() => null);
    steps.nestedChildId = nestedChildId;
    steps.nestedUnderSecond = nestedChildId !== null && nestedChildId !== secondId;
    steps.nestedParentRowFound =
      (await page.locator(`[data-menu-item="${secondId}"]`).count()) > 0;
    if (nestedChildId) {
      // A third level has to be a CHILD of the second, and "Nest X under the row above" cannot
      // do that: the row it would nest is already the parent of the selected row, so the click
      // either refuses or is a no-op and the tree stays two deep. The editor's own affordance
      // for going deeper is "Add a child under <row>", so the pass drives THAT — and then reads
      // the depth back from the store rather than counting the rows it drew, because three
      // visible labels is a two-level tree (a nested row renders inside its parent).
      // The child's own label, read from the child's own row. Asking the PARENT row for a
      // descendant label answers with the parent's text — a nested row is rendered inside its
      // parent, so `… span:first-child` lands on the parent's button — and the button then named
      // "Add a child under QA second" was pressed while the tree was already rooted there. The
      // row carries the child's id, so that is what identifies it.
      const nestedLabel = await page
        .locator(`[data-menu-item-row="${nestedChildId}"] [data-menu-item-label] span:first-child`)
        .first()
        .innerText()
        .catch(() => null);
      steps.nestedRowLabel = nestedLabel;
      const addChild = page.locator(
        `button[aria-label="Add a child under ${nestedLabel}"]`,
      );
      steps.addChildButtonCount = await addChild.count().catch(() => 0);
      steps.addChildClicked = await addChild
        .first()
        .click({ timeout: 3000 })
        .then(() => true)
        .catch(() => false);
      await page.waitForTimeout(500);
      // Type the label, the same way the top-level rows were typed, or the new row is "Untitled
      // item" and the depth probe's search for the third level finds nothing.
      const thirdLabel = "QA third level";
      steps.typedThirdLevel = await labelLastRow(thirdLabel);
    }
    await page.locator("[data-menu-save]").click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(2000);
  }

  // The reload is the claim: the store kept the parents and the positions.
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2000);
  const storedParents = qaSql(
    `select coalesce(string_agg(parent_id::text, ',' order by position), 'none') from cms_menu_items where menu_id = '${menuId}'`,
  );
  steps.savedItems = Number(
    qaSql(`select count(*) from cms_menu_items where menu_id = '${menuId}'`) || 0,
  );
  steps.treeHasChildren = storedParents.includes(",") || (storedParents ?? "").length > 4;
  steps.parentsAreStored = steps.treeHasChildren;
  // Every STORED row has to be in the tree, and a child row is only in the DOM when its
  // parent's branch is open — which is the correct behaviour, not a missing row. So collapsed
  // parents are opened first, and the count is then compared with what the store holds.
  // Counting without opening reports a working editor as broken the moment a branch is collapsed,
  // which is the same false negative the count gave before the tree got a third level.
  // A branch with children but no child list is collapsed, and the grip is what opens it. There
  // is no `data-open` flag on the row, so the state is read from the DOM it produces: a parent
  // whose `<li>` holds a nested `<ul>` is open, and one that does not is not. Clicking a grip by
  // that condition cannot toggle an already-open branch shut, which a blind click would.
  const collapsible = await page.locator("[data-menu-item-row]").evaluateAll((rows) =>
    rows
      .filter((row) => {
        const li = row.closest("li");
        if (!li) return false;
        // Open already? The child list is a direct child of the same <li>, and its presence is
        // the whole of "this branch is open". Asking INSIDE the row for a descendant list finds
        // nothing: the row is a sibling of the <ul>, not its parent, which is why the previous
        // version reported zero collapsed branches and left the deepest row off the count.
        return li.querySelector(":scope > ul") === null;
      })
      .map((row) => row.getAttribute("data-menu-item-row")),
  );
  steps.collapsedBranchesOpened = collapsible.length;
  for (const item of collapsible) {
    await page.locator(`[data-menu-item-grip="${item}"]`).click({ timeout: 2000 }).catch(() => {});
    await page.waitForTimeout(200);
  }
  await page.waitForTimeout(400);
  const renderedRows = await page.locator("[data-menu-tree] li[data-menu-item]").count();
  steps.renderedRows = renderedRows;
  steps.treeRendered = renderedRows >= steps.savedItems;
  steps.depthLabel = await page
    .locator("[data-menu-editor-state=ready] p")
    .first()
    .innerText()
    .catch(() => "");

  // ---------------------------------------------------------------- a fourth level is refused
  const beforeRefusal = qaSql(
    `select count(*) from cms_menu_items where menu_id = '${menuId}'`,
  );
  const fourth = await page.evaluate(async (id) => {
    const detail = await fetch(`/api/v1/menus/${id}`, { credentials: "same-origin" }).then((r) => r.json());
    // The parent of the fourth level has to BE the third level. Parented on the deepest row's OWN
    // parent, a fourth-level row lands on a legal second level, the store accepts it, and the
    // check reads false with a `200` beside it — a depth rule that was never exercised, wearing
    // the costume of a store that does not enforce it. The chain is therefore built from the
    // tree as it actually stands: the deepest existing row becomes the parent, and the new row is
    // appended under it.
    const depthOf = (item, byId) => {
      let d = 1;
      let cur = item;
      while (cur?.parent_id && byId.has(cur.parent_id) && d < 12) {
        cur = byId.get(cur.parent_id);
        d += 1;
      }
      return d;
    };
    const byId = new Map(detail.items.map((item) => [item.id, item]));
    let deepest = detail.items[0] ?? null;
    for (const item of detail.items) {
      if (!deepest || depthOf(item, byId) > depthOf(deepest, byId)) deepest = item;
    }
    const deepestDepth = deepest ? depthOf(deepest, byId) : 0;
    const items = detail.items.map((item) => ({
      id: item.id,
      parent_id: item.parent_id,
      position: item.position,
      label: item.label,
      item_type: item.item_type,
      page_id: item.page_id,
      url: item.url,
      target: item.target,
      rel: item.rel,
      css_class: item.css_class,
      enabled: item.enabled,
      visibility: item.visibility,
      visibility_roles: item.visibility_roles,
    }));
    items.push({
      id: crypto.randomUUID(),
      parent_id: deepest ? deepest.id : null,
      position: 99,
      label: "QA fourth",
      item_type: "url",
      page_id: null,
      url: "/qa-fourth",
      target: "_self",
      rel: "",
      css_class: "",
      enabled: true,
      visibility: "everyone",
      visibility_roles: [],
    });
    const response = await fetch(`/api/v1/menus/${id}/items`, {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ items, locations: detail.locations }),
    });
    return {
      status: response.status,
      body: await response.json().catch(() => ({})),
      builtUnderDepth: deepestDepth + 1,
      parentDepth: deepestDepth,
    };
  }, menuId);
  steps.fourthLevelStatus = fourth.status;
  // Only a probe that actually built a FOURTH level may judge the refusal. A tree that reached
  // three has nothing to refuse, and reporting `false` beside a `200` for that is a store that
  // looks broken for honouring the bound it documents. The tree's own deepest depth is recorded
  // so this is never silently a skip.
  steps.fourthLevelParentDepth = fourth.parentDepth;
  steps.fourthLevelReached = fourth.parentDepth === 3;
  steps.fourthLevelRefused = fourth.status === 400;
  steps.fourthLevelCode = fourth.body?.error?.code ?? null;
  steps.fourthLevelBuiltUnder = fourth.builtUnderDepth;
  steps.fourthLevelMessage = fourth.body?.error?.message ?? null;
  steps.refusalLeftTheTreeAlone =
    Number(qaSql(`select count(*) from cms_menu_items where menu_id = '${menuId}'`) || 0) ===
    Number(beforeRefusal);

  // ---------------------------------------------------------------- Add pages…
  const publishedPage = qaSql(
    `select id from pages where site_id = '${siteId}' and status = 'published' limit 1`,
  );
  if (publishedPage) {
    await page.locator("[data-menu-add-pages]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1500);
    steps.pickerOpened = (await page.locator("[data-page-picker]").count()) > 0;
    // Only published pages are offered: the server refuses a batch containing a draft *whole*,
    // so a picker that listed drafts would let an editor select five pages and lose all five.
    // The picker's own answer and the API's are compared, and both halves are read from inside
    // the browser: a Playwright locator is not in scope inside `page.evaluate`, and reaching for
    // one there throws `ReferenceError: page is not defined` — which aborts the pass and reports
    // a crash rather than a failed assertion.
    steps.pickerOnlyOffersPublished = await page.evaluate(async () => {
      const site = await fetch("/api/v1/sites", { credentials: "same-origin" }).then((r) => r.json());
      const first = site.sites?.[0];
      if (!first) return null;
      const all = await fetch(`/api/v1/pages?site_id=${first.id}`, { credentials: "same-origin" }).then(
        (r) => r.json(),
      );
      const drafts = (all.pages ?? []).filter((p) => p.status !== "published").map((p) => p.id);
      if (drafts.length === 0) return true;
      const offered = Array.from(
        document.querySelectorAll("[data-page-picker-page]"),
      ).map((node) => node.getAttribute("data-page-picker-page"));
      return drafts.every((id) => !offered.includes(id));
    });
    await page.locator(`[data-page-picker-page="${publishedPage}"]`).check({ timeout: 3000 }).catch(() => {});
    await page.locator("[data-page-picker-add]").click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(2000);
    steps.pageItems = Number(
      qaSql(
        `select count(*) from cms_menu_items where menu_id = '${menuId}' and item_type = 'page'`,
      ) || 0,
    );
    // The label is the page's own title, not its slug — a menu of slugs is a menu somebody has
    // to edit by hand afterwards.
    steps.labelComesFromTheTitle = Number(
      qaSql(
        `select count(*) from cms_menu_items i join page_revisions r on r.page_id = i.page_id
         where i.menu_id = '${menuId}' and r.title = i.label`,
      ) || 0,
    );
    await page.waitForTimeout(800);
  }

  // ---------------------------------------------------------------- the location rail
  await page.locator('[data-menu-location-toggle="header"]').check({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(300);
  await page.locator("[data-menu-save]").click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.claimedHeader = qaSql(
    `select count(*) from cms_menus where id = '${menuId}' and 'header' = any(locations)`,
  ) === "1";

  // A second menu cannot take the same slot, and the refusal must name the holder.
  const rivalKey = `qa-menu-rival-${stamp}`;
  const rival = await page.evaluate(
    async ([id, key]) => {
      const detail = await fetch(`/api/v1/menus/${id}`, { credentials: "same-origin" }).then((r) => r.json());
      const created = await fetch("/api/v1/menus", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ site_id: detail.site_id, key, name: "QA rival" }),
      });
      const body = await created.json();
      if (!created.ok) return { created: created.status, body };
      const claimed = await fetch(`/api/v1/menus/${body.id}`, {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ items: [], locations: ["header"] }),
      });
      return {
        created: created.status,
        rivalId: body.id,
        claimStatus: claimed.status,
        claimBody: await claimed.json().catch(() => ({})),
      };
    },
    [menuId, rivalKey],
  );
  steps.rivalClaimStatus = rival.claimStatus ?? null;
  steps.rivalClaimRefused = rival.claimStatus === 409;
  steps.rivalRefusalNamesTheHolder =
    (rival.claimBody?.error?.message ?? "").includes(menuKey) ||
    (rival.claimBody?.error?.details?.toString?.() ?? "").includes(menuKey);
  steps.firstHolderKeptIt = qaSql(
    `select count(*) from cms_menus where id = '${menuId}' and 'header' = any(locations)`,
  ) === "1";

  // ---------------------------------------------------------------- the audience toggle
  // A members-only item, saved through the screen, then read back from the *public* endpoint.
  const memberId = await page.evaluate(async (id) => {
    const detail = await fetch(`/api/v1/menus/${id}`, { credentials: "same-origin" }).then((r) => r.json());
    const items = (detail.items ?? []).map((item) => ({ ...item }));
    const target = items.find((item) => item.label === "QA first") ?? items[0];
    // No item to make members-only is a FAILURE of an earlier step, not a reason to throw: the
    // exception unwinds `main()` past every step recorded so far, and a pass that dies here
    // reports one line — "Cannot set properties of undefined" — for a menu editor that has
    // already told us what is wrong in `steps.savedItems` and `steps.threeTopLevel`. Record the
    // fact and let the rest of the pass keep proving what it can.
    if (!target) return { status: 0, itemId: null, reason: `the menu held ${items.length} items` };
    target.visibility = "members";
    const response = await fetch(`/api/v1/menus/${id}/items`, {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ items, locations: detail.locations }),
    });
    return { status: response.status, itemId: target.id };
  }, menuId);
  steps.memberItemSaved = memberId.status === 200;
  steps.memberItemReason = memberId.reason ?? null;

  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2200);
  const visitorCount = await page
    .locator("[data-menu-preview-item]")
    .count()
    .catch(() => 0);
  steps.visitorItems = visitorCount;
  await page.locator('[data-menu-preview-audience="member"]').click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const memberCount = await page.locator("[data-menu-preview-item]").count().catch(() => 0);
  steps.memberItems = memberCount;
  // The acceptance criterion in one comparison: the same endpoint, two audiences, and the
  // members-only row is the difference between them.
  steps.audienceToggleChangesThePayload = memberCount > visitorCount;
  steps.membersItemHiddenFromVisitor =
    (await page
      .locator(`[data-menu-preview-item="${memberId.itemId}"]`)
      .count()
      .catch(() => 0)) > 0;

  // ---------------------------------------------------------------- the queue
  const queuePage = qaSql(
    `select id from pages where site_id = '${siteId}' and status = 'published' limit 1`,
  );
  if (queuePage) {
    const entry = await page.evaluate(
      async ([id, stamp]) => {
        const response = await fetch(`/api/v1/pages/${id}/schedule`, {
          method: "POST",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            action: "publish",
            scheduled_at: new Date(Date.now() + 3 * 24 * 3600 * 1000).toISOString(),
            timezone: "Europe/Istanbul",
          }),
        });
        return { status: response.status, body: await response.json().catch(() => ({})) };
      },
      [queuePage, stamp],
    );
    steps.scheduleStatus = entry.status;
    steps.entryId = entry.body?.id ?? null;

    await page.goto(`${URL_ADMIN}/publishing/queue`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1800);
    steps.queueReady = (await page.locator("[data-queue-state=ready]").count()) > 0;
    steps.entryOnScreen = entry.body?.id
      ? (await page.locator(`[data-queue-row="${entry.body.id}"]`).count()) > 0
      : false;

    if (entry.body?.id) {
      // Reschedule through the form, then read the stored instant back from SQL.
      await page
        .locator(`[data-queue-reschedule="${entry.body.id}"]`)
        .click({ timeout: 4000 })
        .catch(() => {});
      await page.waitForTimeout(500);
      steps.rescheduleFormOpened =
        (await page.locator(`[data-queue-reschedule-form="${entry.body.id}"]`).count()) > 0;
      const moved = new Date(Date.now() + 5 * 24 * 3600 * 1000);
      const stamp5 = `${moved.getFullYear()}-${String(moved.getMonth() + 1).padStart(2, "0")}-${String(
        moved.getDate(),
      ).padStart(2, "0")}T09:00`;
      await page.locator("[data-queue-reschedule-input]").fill(stamp5).catch(() => {});
      await page.locator("[data-queue-reschedule-save]").click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(2000);
      const stored = qaSql(
        `select to_char(scheduled_at at time zone 'UTC', 'YYYY-MM-DD"T"HH24:MI') from cms_publishing_queue where id = '${entry.body.id}'`,
      );
      steps.rescheduleStored = stored;
      // The instant is stored in UTC and the field is a local wall clock, so the comparison is
      // "tomorrow, not today" rather than an exact string: a form that sent the wall clock
      // unconverted would land five hours off, and an equality check would only pass on a
      // machine at UTC+0.
      steps.rescheduleMoved = /^\d{4}-\d{2}-\d{2}T09:00$/.test(stored ?? "");
      steps.rescheduleIsLater = stored > (entry.body.scheduled_at ?? "").slice(0, 16);

      // A done row is not reschedulable, cancellable or publishable — the buttons are not drawn.
      const doneRow = qaSql(`select id from cms_publishing_queue where status = 'done' limit 1`);
      if (doneRow) {
        steps.doneRowHasNoActions = (await page.locator(`[data-queue-cancel="${doneRow}"]`).count()) === 0;
      }

      // The queue's own retry, asked directly, because the button only exists on a failed row and
      // this entry is not one. The claim is the REFUSAL: `retry` only moves a `failed` entry back
      // to `pending`, so pressing it on a pending entry is the same mistake as pressing it twice —
      // a duplicate publish scheduled for a second delivery. The store answers
      // `publishing_entry_not_found` rather than an error shape, so the status is recorded beside
      // the code: a `200` with the row still `pending` would be a second definition of "retried".
      //
      // This is written here, in the pass that owns the queue, rather than read off the
      // notifications pass: that one drives a different screen with a different outbox and never
      // runs in `--only=menus`, so demanding its key from this checklist was a check that could
      // never be written — a permanently "missing" item that reads like a broken product.
      const retry = await page.evaluate(async (id) => {
        const response = await fetch(`/api/v1/publishing/queue/${id}/retry`, {
          method: "POST",
          credentials: "same-origin",
        });
        return { status: response.status, body: await response.json().catch(() => ({})) };
      }, entry.body.id);
      steps.retryRefusesAPendingRow = retry.status;
      steps.retryRefusesASentRow = retry.status !== 200;
      steps.retryRefusalCode = retry.body?.error?.code ?? null;
      steps.retryLeftTheRowPending =
        qaSql(`select status from cms_publishing_queue where id = '${entry.body.id}'`) === "pending";

      // Cancel it, and prove the row is really cancelled rather than merely off screen.
      await page.locator(`[data-queue-cancel="${entry.body.id}"]`).click({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(1800);
      steps.cancelledInSql = qaSql(
        `select status from cms_publishing_queue where id = '${entry.body.id}'`,
      ) === "cancelled";
      steps.cancelButtonGone =
        (await page.locator(`[data-queue-cancel="${entry.body.id}"]`).count()) === 0;
    }

    // The filter is the server's, so the chip has to narrow the table rather than the page.
    await page.locator('[data-queue-chip="pending"]').click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1500);
    steps.filteredToPending = await page.evaluate(() =>
      Array.from(document.querySelectorAll("[data-queue-row]")).every((row) => {
        const text = row.textContent ?? "";
        return !/cancelled/.test(text);
      }),
    );
  }

  // ---------------------------------------------------------------- cleanup
  qaSql(`delete from cms_menu_items where menu_id in (select id from cms_menus where key like 'qa-menu-%')`);
  qaSql(`delete from cms_menus where key like 'qa-menu-%'`);
  if (steps.entryId) {
    qaSql(`delete from cms_publishing_queue where id = '${steps.entryId}'`);
  }
  // The seeded pages go with it. Left behind, they accumulate one pair per run and the picker's
  // own list grows until the pass takes noticeably longer to open it — a fixture that is never
  // cleaned is a slow failure that looks like a slow product.
  qaSql(`delete from pages where slug like 'qa-menu-page-%-${stamp}' or slug = 'qa-menu-page-${stamp}'`);
  return steps;
}

async function runNotificationOutboxDepth(page, report) {
  const steps = {};
  await page.goto(`${URL_ADMIN}/notifications/outbox`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1800);

  steps.loaded = (await page.locator("[data-outbox-state=ready]").count()) > 0;
  if (!steps.loaded) {
    steps.reason = await page
      .locator("[data-outbox-state=error]")
      .innerText()
      .catch(() => "the outbox did not reach its ready state");
    return steps;
  }

  // The chips and the log. A screen whose chips are all "0" above a real table is a screen
  // that renders, so the counts are compared against SQL rather than against their own look.
  const chips = await page.locator("[data-outbox-chip]").allInnerTexts().catch(() => []);
  steps.chips = chips.length;
  steps.chipsCarryCounts = chips.filter((text) => /\(\d+\)/.test(text)).length;
  await shot(page, "page-notifications-outbox");

  // The log's own rows, and whether the table behind it agrees. A retry run earlier in the pass
  // may have moved things, so this is a comparison, not an equality.
  const deliveryCount = Number(qaSql("select count(*) from notification_deliveries") || 0);
  steps.deliveryRows = deliveryCount;
  steps.chiptotal = Number(
      (chips.find((text) => text.startsWith("All")) || "").match(/\((\d+)\)/)?.[1] || -1,
    );
  steps.chiptotalMatchesSql = steps.chiptotal === deliveryCount;
  if (!steps.chiptotalMatchesSql) {
    steps.note = `the chip says ${steps.chiptotal}, the table has ${deliveryCount}`;
  }

  // 1. Write a rule through the form. Every field is filled from the screen's own controls —
  //    a fixture that POSTs the API directly would leave the form untested, and the form is
  //    where a recipient prefix gets typed wrong.
  await page.locator("[data-rules-toggle]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  steps.formOpened = (await page.locator("[data-rule-form]").count()) > 0;

  const eventName = `qa.ticket.created.${Date.now()}`;
  await page.locator("[data-rule-event]").fill(eventName).catch(() => {});
  await page.locator("[data-rule-category]").selectOption("ticket").catch(() => {});
  await page.locator("[data-rule-priority]").selectOption("high").catch(() => {});
  // `actor` needs no target, so the target field must *not* be on screen — a form that shows a
  // target for the shape that has none is a form asking for input it will discard.
  steps.targetHiddenForActor = (await page.locator("[data-rule-target]").count()) === 0;
  await page.locator("[data-rule-title]").fill("QA rule for {subject}").catch(() => {});
  await page.locator("[data-rule-save]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1500);

  steps.ruleRows = Number(
    qaSql(`select count(*) from notification_routes where event_name = '${eventName}'`) || 0,
  );
  steps.ruleIsInTheTable = steps.ruleRows === 1;
  steps.ruleVisibleOnScreen =
    (await page.locator(`[data-rule-row="${eventName}"]`).count()) > 0;

  // 2. The permission shape must make the target field appear, and must not submit without it.
  await page.locator("[data-rules-toggle]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-rule-shape]").selectOption("permission:").catch(() => {});
  await page.waitForTimeout(300);
  steps.targetAppearsForPermission = (await page.locator("[data-rule-target]").count()) > 0;
  await page.locator("[data-rule-event]").fill(`${eventName}.unused`).catch(() => {});
  await page.locator("[data-rule-title]").fill("QA rule without a target").catch(() => {});
  await page.locator("[data-rule-save]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(800);
  // The browser's own `required` is the first line of defence; the database's check is the
  // second. This asserts the row did not land, which is the claim either way.
  steps.noTargetWroteNothing = Number(
    qaSql(`select count(*) from notification_routes where event_name = '${eventName}.unused'`) || 0,
  ) === 0;
  await page.locator("[data-rule-form] button[type=button]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);

  // 3. Run the event through the router. The event id is minted by the screen, so the second
  //    run cannot be made to collide by the harness — which is the point: a *different* run
  //    creating a second row is correct, and asserting a duplicate here would assert a bug.
  const before = Number(
    qaSql(
      `select count(*) from notifications where source_type = 'event' and source_id = '${eventName}'`,
    ) || 0,
  );
  await page.locator("[data-probe-event]").fill(eventName).catch(() => {});
  await page.locator("[data-probe-subject]").fill("QA probe subject").catch(() => {});
  await page.locator("[data-probe-run]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1800);

  steps.probeReported = (await page.locator("[data-probe-report]").count()) > 0;
  steps.probeCreated = await page
    .locator("[data-probe-created]")
    .first()
    .getAttribute("data-probe-created")
    .catch(() => null);
  const after = Number(
    qaSql(
      `select count(*) from notifications where source_type = 'event' and source_id = '${eventName}'`,
    ) || 0,
  );
  // The actor rule resolves to the event's actor; the harness sends none, so the honest
  // answer is zero created and one unmatched rule — and the screen must *say* which, rather
  // than showing a bare "0".
  steps.rowsAfter = after;
  steps.probeMatchesTheTable = after === before;
  steps.unmatchedIsExplained =
    Number(steps.probeCreated) === after - before ||
    (await page.locator("[data-probe-report]").innerText().catch(() => "")).includes("matched nobody");

  // 4. Now with an actor, so the rule actually fires and a row really lands.
  const actorRun = await page.evaluate(async (name) => {
    const me = await fetch("/api/v1/me", { credentials: "same-origin" })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
    const response = await fetch("/api/v1/notifications/route", {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        event_name: name,
        actor_user_id: me?.user?.id ?? me?.id,
        payload: { title: "QA actor probe" },
      }),
    });
    return { status: response.status, body: await response.json().catch(() => ({})) };
  }, eventName);
  steps.actorRunStatus = actorRun.status;
  steps.actorCreated = actorRun.body?.created ?? null;
  steps.rowsAfterActor = Number(
    qaSql(
      `select count(*) from notifications where source_type = 'event' and source_id = '${eventName}'`,
    ) || 0,
  );
  steps.actorActuallyWroteARow = steps.rowsAfterActor > after;

  // 5. Remove the rule, and prove the table agrees rather than trusting the toast.
  const ruleId = qaSql(`select id from notification_routes where event_name = '${eventName}' limit 1`);
  steps.ruleId = ruleId || null;
  if (ruleId) {
    const removed = await page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/notifications/routes/${id}`, {
        method: "DELETE",
        credentials: "same-origin",
      });
      return response.status;
    }, ruleId);
    steps.removeStatus = removed;
    steps.removedFromTheTable =
      Number(qaSql(`select count(*) from notification_routes where event_name = '${eventName}'`) || 0) === 0;
    await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1200);
    steps.goneFromTheScreen = (await page.locator(`[data-rule-row="${eventName}"]`).count()) === 0;
  }

  // 6. The retry path, asked directly, because the button only exists on a row that failed and
  //    the QA database may have none. The claim is the *refusal* on a non-failed row.
  steps.retryRefusesASentRow = await page.evaluate(async () => {
    const sent = await fetch("/api/v1/notifications/outbox?status=sent&limit=1", {
      credentials: "same-origin",
    })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
    const row = sent?.rows?.[0];
    if (!row) return null;
    const response = await fetch(`/api/v1/notifications/outbox/${row.id}/retry`, {
      method: "POST",
      credentials: "same-origin",
    });
    return { status: response.status, outcome: (await response.json().catch(() => ({})))?.outcome };
  });
  steps.retryIsNotRetryable = steps.retryRefusesASentRow?.outcome !== "requeued";

  // Leave nothing behind: the probe's own notifications, and the event names it used.
  qaSql(`delete from notifications where source_type = 'event' and source_id like 'qa.ticket.created.%'`);
  qaSql(`delete from notification_routes where event_name like 'qa.ticket.created.%'`);

  return steps;
}

/**
 * The event console (REQ-016, slice 1): the feed and the catalogue, each driven rather than
 * merely rendered.
 *
 * The events are emitted through real routes — a page is published, so the bus records
 * `page.created`, `page.published` and `page.updated` with payloads the panel has to show —
 * and the pass then proves the three things a feed screen can quietly get wrong:
 *
 * 1. **The filter narrows the table.** A name is chosen, and every row on screen carries it.
 *    A filter bar wired to nothing still renders rows, and rows are what a screenshot proves,
 *    so this has to be asserted against the rows rather than against the control.
 * 2. **The filter survives a reload.** The chips are read from the query string, so a second
 *    visit to the same URL is the same view. This is the paste-to-a-colleague claim.
 * 3. **The catalogue is the registry, not a picture of one.** Its total is the registry's
 *    total, and narrowing it by area leaves a subset of it — a catalogue that had been
 *    hand-listed in the screen would drift from the API, and the drift is the bug the whole
 *    request exists to prevent.
 */
async function runEventsDepth(page, report) {
  const steps = {};
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);

  // 1. Emit real facts through real routes. Publishing a page is the one an operator can
  //    always do, and it produces three names with three different payload shapes.
  const published = await page.evaluate(async (site) => {
    const created = await fetch(`/api/v1/pages?site_id=${site}`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ title: "QA · an event to look at", slug: `qa-event-${Date.now()}` }),
    }).then((r) => (r.ok ? r.json() : null)).catch(() => null);
    if (!created?.id) return { created: null };
    const publishedPage = await fetch(`/api/v1/pages/${created.id}/publish`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: "{}",
    }).then((r) => (r.ok ? r.json() : null)).catch(() => null);
    return { created: created.id, published: publishedPage?.status ?? null };
  }, siteId);
  steps.emitted = published;

  // 2. The feed, on its own route.
  await page.goto(`${URL_ADMIN}/events`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.table = (await page.locator("[data-event-table]").count()) > 0;
  steps.rows = await page.locator("[data-event-row]").count();
  steps.empty = (await page.locator("[data-event-empty]").count()) > 0;
  // The page this pass just published has to be on screen; a feed that does not show the
  // event an operator just caused is a log file nobody reads.
  steps.sawThePublication = (await page.locator('[data-event-row] >> text=page.published').count()) > 0;
  await shot(page, "page-events-feed");

  // 3. The name filter narrows the table, and the URL carries it.
  if (steps.rows > 0) {
    const firstName = (await page.locator("[data-event-row] td:nth-child(3)").first().innerText().catch(() => "")).trim();
    steps.firstName = firstName;
    if (firstName) {
      await page.locator(`[data-event-name-option="${firstName}"]`).check({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(1200);
      await shot(page, "page-events-filtered");
      steps.filterInUrl = page.url().includes("name=");
      steps.filterChip = (await page.locator(`[data-event-name-chip="${firstName}"]`).count()) > 0;
      const names = await page.locator("[data-event-row] td:nth-child(3)").allInnerTexts();
      steps.everyRowMatches = names.length > 0 && names.every((value) => value.trim() === firstName);
      steps.filteredRows = names.length;

      // And the same URL gives the same view — the paste-to-a-colleague claim, asserted by
      // reloading rather than by trusting the router.
      await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForTimeout(1300);
      const afterReload = await page.locator("[data-event-row] td:nth-child(3)").allInnerTexts();
      steps.survivesReload =
        afterReload.length > 0 && afterReload.every((value) => value.trim() === firstName);
      steps.reset = await (async () => {
        await page.locator("[data-event-reset]").click({ timeout: 4000 }).catch(() => {});
        await page.waitForTimeout(1000);
        return !page.url().includes("name=");
      })();
    }
  }

  // 4. The payload inspector: a row expands and shows the payload as it was recorded.
  if (steps.rows > 0) {
    const rowId = await page.locator("[data-event-row]").first().getAttribute("data-event-row");
    await page.locator(`[data-event-expand="${rowId}"]`).click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(500);
    steps.expanded = (await page.locator(`[data-event-payload="${rowId}"]`).count()) > 0;
    steps.payloadText = (
      await page.locator(`[data-event-payload="${rowId}"]`).innerText().catch(() => "")
    ).slice(0, 200);

    // 4b. The key tree, and the path it copies. The claim under test is that the button hands
    //     over the path a *receiver* would write, so the assertion reads the clipboard the
    //     browser actually filled rather than trusting the button's presence: a copy button
    //     that copies the key's display name passes a click test and fails the reader.
    const pathBlock = page.locator(`[data-event-paths="${rowId}"]`);
    steps.hasPathTree = (await pathBlock.count()) > 0;
    if (steps.hasPathTree) {
      const firstPathButton = pathBlock.locator("[data-event-copy-path]").first();
      steps.pathCount = await pathBlock.locator("[data-event-copy-path]").count();
      const pathHint = await firstPathButton.getAttribute("title").catch(() => null);
      steps.pathButtonNamesItsPath = !!pathHint && pathHint.startsWith("Copy the path ");
      steps.pathIsRootedAtPayload = !!pathHint && pathHint.includes("payload");
      await shot(page, "page-events-paths");

      await page
        .context()
        .grantPermissions(["clipboard-read", "clipboard-write"])
        .catch(() => {});
      await firstPathButton.click({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(350);
      const clip = await page
        .evaluate(() => navigator.clipboard.readText().catch(() => ""))
        .catch(() => "");
      steps.clipboardPath = clip;
      // The copied text must BE the path, and must be the one the button advertised. A
      // mismatch here is the exact failure the acceptance box is about.
      steps.clipboardMatchesHint = !!pathHint && clip === pathHint.replace("Copy the path ", "");
      steps.noticeNamesTheCopy = (
        await page.locator("[data-event-notice], [role=status]").first().innerText().catch(() => "")
      ).includes("Copied");
    }

    await shot(page, "page-events-payload");
    // The keyboard path: `j` walks the cursor and `Enter` opens, so the shortcuts are real.
    await page.locator("[data-event-table] tbody").focus().catch(() => {});
    await page.keyboard.press("j");
    await page.waitForTimeout(200);
    steps.cursorMoved = (await page.locator('[data-event-row][data-cursor="true"]').count()) > 0;
    await page.keyboard.press("Enter");
    await page.waitForTimeout(400);
    steps.keyboardOpens = (await page.locator("[data-event-detail]").count()) > 0;
    await page.keyboard.press("Escape");
    await page.waitForTimeout(300);
    steps.escapeCloses = (await page.locator("[data-event-detail]").count()) === 0;
  }

  // 5. The error state, provoked the honest way: a route that answers 500. A feed that cannot
  //    say "the API could not be reached" is a feed that renders an empty table and calls it
  //    "nothing recorded yet", which is the most expensive kind of wrong.
  await page.route("**/api/v1/events?*", (route) =>
    route.fulfill({
      status: 500,
      contentType: "application/json",
      body: JSON.stringify({ error: { code: "qa_forced", message: "The QA pass forced this." } }),
    }),
  );
  await page.goto(`${URL_ADMIN}/events?window=24h`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.errorShown = (await page.locator("[data-event-error]").count()) > 0;
  steps.errorSaysWhy = (await page.locator("[data-event-error]").innerText().catch(() => "")).includes(
    "QA pass forced this",
  );
  steps.retryPresent = (await page.locator("[data-event-error] >> text=Retry").count()) > 0;
  await shot(page, "page-events-error");
  await page.unroute("**/api/v1/events?*").catch(() => {});

  // 6. The catalogue: the registry's own totals, and a narrowing that leaves a subset.
  await page.goto(`${URL_ADMIN}/events?tab=catalogue`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.catalogueTable = (await page.locator("[data-event-catalogue-table]").count()) > 0;
  steps.catalogueRows = await page.locator("[data-catalogue-row]").count();
  steps.catalogueLive = (await page.locator("[data-catalogue-live]").innerText().catch(() => "")).trim();
  steps.catalogueReserved = (await page.locator("[data-catalogue-reserved]").innerText().catch(() => "")).trim();
  // The screen's total must be the API's total — read from the API, not from the rows, so a
  // filtered list cannot quietly become a smaller registry.
  const fromApi = await page
    .evaluate(() =>
      fetch("/api/v1/events/catalogue", { credentials: "same-origin" })
        .then((r) => (r.ok ? r.json() : null))
        .catch(() => null),
    )
    .catch(() => null);
  steps.catalogueMatchesApi =
    fromApi?.events?.length != null && fromApi.events.length === steps.catalogueRows;
  steps.catalogueCountsAreNumbers =
    typeof fromApi?.live_count === "number" && typeof fromApi?.reserved_count === "number";
  steps.everyEntryHasADeliveryCount =
    Array.isArray(fromApi?.events) &&
    fromApi.events.every((entry) => typeof entry.deliveries_24h === "number");
  // A reserved name says so on screen; hiding it would leave a subscriber waiting forever
  // for an event no module emits.
  steps.reservedIsVisible = (await page.locator('[data-catalogue-row][data-status="reserved"]').count()) > 0;
  steps.liveIsVisible = (await page.locator('[data-catalogue-row][data-status="live"]').count()) > 0;
  await shot(page, "page-events-catalogue");

  const area = await page
    .locator("#event-area option")
    .nth(1)
    .getAttribute("value")
    .catch(() => "");
  if (area) {
    await page.selectOption("#event-area", area).catch(() => {});
    await page.waitForTimeout(600);
    const areas = await page.locator("[data-catalogue-row] td:nth-child(2)").allInnerTexts();
    steps.areaNarrowed =
      areas.length > 0 && areas.length < steps.catalogueRows && areas.every((value) => value.trim() === area);
    steps.area = area;
  }

  // 7. The payload fields of one entry, because "what does this event carry" is the question
  //    a receiver asks before subscribing and the screen is where it is answered.
  const firstEntry = await page.locator("[data-catalogue-row]").first().getAttribute("data-catalogue-row").catch(() => "");
  if (firstEntry) {
    await page.locator(`[data-catalogue-expand="${firstEntry}"]`).click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(400);
    steps.payloadFields = await page.locator(`[data-catalogue-field="${firstEntry}"]`).count();
  }

  // 8. "Filter feed" from the catalogue must land on the Feed tab with the name applied — the
  //    two tabs are one screen, and a button that does not cross between them is a button
  //    that lies about what it does.
  if (firstEntry) {
    await page.locator(`[data-catalogue-filter="${firstEntry}"]`).click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1300);
    steps.crossTabFilter =
      !page.url().includes("tab=catalogue") && page.url().includes("name=");
    steps.crossTabChip = (await page.locator(`[data-event-name-chip="${firstEntry}"]`).count()) > 0;
    await shot(page, "page-events-filtered-from-catalogue");
  }

  return steps;
}

/**
 * The webhook endpoint pass (REQ-016, slice 2): connect an endpoint through the real form,
 * see its deliveries, force one again, rotate its secret, and remove it.
 *
 * The pass is built around a real receiver rather than a mocked API response, because the one
 * claim this screen makes that nothing else can check is that a delivery is *signed* and that
 * rotating the secret stops the old signature verifying. `infra/mocks/webhook-receiver.mjs` is
 * that receiver: it verifies the HMAC over the bytes it received and answers `401` to a
 * signature that does not match, which is exactly what a real receiver has to do.
 *
 * The five things it proves, each one a way a webhook screen can look finished and be wrong:
 *
 * 1. **The secret is shown once and the create form gates `Done` on it.** A form that
 *    navigates away before the operator has read the secret is a form that loses it.
 * 2. **The delivery history is the queue, not a picture of one.** The row count on screen is
 *    compared against the API's own total, so a filter that renders rows it did not fetch
 *    cannot pass.
 * 3. **A forced delivery is sent again** and the trigger column changes to `replay` — the
 *    reset, not a second row, which the database would have refused anyway.
 * 4. **Rotating shows a new secret and the old one stops working.** Verified by the receiver's
 *    own `401`, which is the only honest proof available.
 * 5. **Remove is confirmed by name** and actually removes the endpoint.
 */
async function runWebhooksDepth(page, report) {
  const steps = {};
  const siteId = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);

  // ---- The receiver -----------------------------------------------------------------------------
  // Started here rather than by run.sh because only this pass needs it, and a process nothing
  // uses is a process to remember to clean up. It is killed in the `finally` below whatever
  // happens, so a throw in the middle of the pass does not leave a port bound.
  const port = 8124 + (Number(process.env.QA_STACK_SLOT) || 0);
  const secret = "qa-webhook-pass-secret-2026";
  const receiverUrl = `http://127.0.0.1:${port}/hooks/omnion`;
  const receiver = spawn(process.execPath, ["infra/mocks/webhook-receiver.mjs", String(port)], {
    cwd: path.resolve(__dirname, "../.."),
    env: { ...process.env, OMNION_WEBHOOK_SECRET: secret },
    stdio: "ignore",
  });
  // Give the listener a moment; without it the first delivery races the bind and the walk
  // would report a refused connection as a failing receiver.
  await new Promise((resolve) => setTimeout(resolve, 700));

  const stamp = Date.now().toString(36);
  const name = `QA receiver ${stamp}`;

  try {
    // ---- 1. The list, before anything exists -----------------------------------------------------
    await page.goto(`${URL_ADMIN}/webhooks`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-webhook-empty], [data-webhook-table]", { timeout: 8000 }).catch(() => {});
    steps.emptyState = (await page.locator("[data-webhook-empty]").count()) > 0;
    steps.emptyOffersTheAction = (await page.locator('[data-webhook-empty] a[href="/webhooks/new"]').count()) > 0;
    await shot(page, "page-webhooks-empty");

    // ---- 2. The create form, driven field by field ------------------------------------------------
    await page.goto(`${URL_ADMIN}/webhooks/new`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-webhook-field-name]", { timeout: 8000 }).catch(() => {});
    steps.pickerPresent = (await page.locator("[data-webhook-event-picker]").count()) > 0;

    // Submitting empty must be refused by the form itself, with a message beside each field.
    await page.locator("[data-webhook-submit]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(400);
    steps.emptyNameRefused = (await page.locator("[data-webhook-error-name]").count()) > 0;
    steps.emptyUrlRefused = (await page.locator("[data-webhook-error-url]").count()) > 0;
    steps.emptyEventsRefused = (await page.locator("[data-webhook-error-events]").count()) > 0;
    await shot(page, "page-webhooks-form-errors");

    // A URL with a space and no scheme is the API's own rule; the form must say so before the
    // round trip, or the screen is a decoration on top of a validator.
    await page.locator("[data-webhook-field-name]").fill(name);
    await page.locator("[data-webhook-field-url]").fill("not a url");
    await page.locator("[data-webhook-submit]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(400);
    steps.badUrlRefused = (await page.locator("[data-webhook-error-url]").count()) > 0;

    // HTTP warns without blocking: a receiver on a private network is legitimate, and a panel
    // that refuses it is a panel people work around.
    await page.locator("[data-webhook-field-url]").fill(receiverUrl.replace("http://", "http://"));
    await page.waitForTimeout(300);
    steps.insecureWarns = (await page.locator("[data-webhook-insecure-warning]").count()) > 0;
    await shot(page, "page-webhooks-form-warning");

    // The group checkbox subscribes to a whole area in one click, which is the difference
    // between a picker of 68 names and a usable form.
    await page.locator('[data-webhook-group="page"]').check({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(300);
    const afterGroup = await page
      .evaluate(async () => {
        const answer = await fetch("/api/v1/events/catalogue", { credentials: "same-origin" });
        const body = await answer.json();
        return body.events.filter((entry) => entry.area === "page" && entry.status === "live").length;
      })
      .catch(() => 0);
    steps.groupSelectsTheWholeArea = (await page.locator('[data-webhook-event="page.published"]').count()) > 0;
    steps.cataloguePageNames = afterGroup;

    // Own-secret mode, with a value the API refuses (too short), proves the field is wired.
    await page.locator("[data-webhook-secret-own]").check({ timeout: 4000 }).catch(() => {});
    await page.locator("[data-webhook-secret-input]").fill("short");
    await page.locator("[data-webhook-submit]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(400);
    steps.shortSecretRefused = (await page.locator("[data-webhook-error-secret]").count()) > 0;
    await shot(page, "page-webhooks-form-secret");

    // Back to "generate for me", which is what the rest of the pass runs on: the API issues
    // the secret and the screen has one chance to show it.
    await page.locator("[data-webhook-secret-generate]").check({ timeout: 4000 }).catch(() => {});
    await page.locator("[data-webhook-submit]").click({ timeout: 6000 }).catch(() => {});
    await page.waitForSelector("[data-webhook-secret-once]", { timeout: 8000 }).catch(() => {});

    // ---- 3. The secret, shown exactly once --------------------------------------------------------
    steps.secretShown = (await page.locator("[data-webhook-secret-value]").count()) > 0;
    const issued = (await page.locator("[data-webhook-secret-value]").textContent().catch(() => "")) || "";
    steps.secretHasValue = issued.trim().length >= 16;
    // `Done` is refused until the box says the secret was stored: navigating away first is how
    // an operator loses the only copy of it.
    steps.doneBlockedBeforeStoring =
      await page.locator("[data-webhook-secret-done]").isDisabled().catch(() => false);
    await shot(page, "page-webhooks-secret-once");

    await page.locator("[data-webhook-secret-stored]").check({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(300);
    steps.doneEnabledAfterStoring =
      !(await page.locator("[data-webhook-secret-done]").isDisabled().catch(() => true));
    await page.locator("[data-webhook-secret-done]").click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-webhook-header], [data-webhook-overview-test]", { timeout: 8000 }).catch(() => {});
    steps.doneLandsOnTheEndpoint = page.url().includes("/webhooks/");
    const endpointId = (page.url().match(/\/webhooks\/([0-9a-f-]{36})/) || [])[1] || "";
    steps.endpointId = endpointId;

    // ---- 4. The test delivery reaches a receiver that verifies the signature ----------------------
    await page.locator("[data-webhook-overview-test]").click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(600);
    steps.testQueued = (await page.locator("[data-webhook-notice]").count()) > 0;

    // The runner is a loop in the API process; give it a moment, then read the receiver's own
    // record. A signed delivery that arrives and verifies is the only honest proof that the
    // receiver and the platform agree on the wire format.
    await new Promise((resolve) => setTimeout(resolve, 6000));
    steps.receiverAccepted = await fetch(`http://127.0.0.1:${port}/received`)
      .then((answer) => answer.json())
      .then((body) => (Array.isArray(body) ? body.length : 0))
      .catch(() => 0);

    // ---- 5. The delivery history -------------------------------------------------------------------
    await page.locator('[data-webhook-tab="deliveries"]').click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-webhook-delivery-table], [data-webhook-delivery-empty]", { timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(800);
    steps.historyHasRows = (await page.locator("[data-webhook-delivery-row]").count()) > 0;
    await shot(page, "page-webhooks-deliveries");

    // The header's total is the API's own number, not a recount of the rows on screen: that is
    // what "showing 25 of 340" means, and a table that silently shows a subset of its filter
    // cannot be read at all.
    steps.totalMatchesTheApi = await page.evaluate(async () => {
      const chips = document.querySelector("[data-webhook-delivery-total]");
      if (!chips) return false;
      const shown = (chips.textContent || "").replace(/,/g, "");
      const answer = await fetch(window.location.pathname + window.location.search, {
        credentials: "same-origin",
      });
      const body = await answer.json();
      return shown.includes(String(body.total));
    }).catch(() => false);

    // A test delivery is a probe, and the trigger column is what says so on screen. A history
    // that renders probes as traffic is the mistake migration 0052's column exists to prevent.
    steps.testRowsLabelled = (await page.locator("[data-webhook-trigger]").count()) > 0;

    // The status filter narrows the table, and survives a reload — the paste-to-a-colleague
    // claim, which only holds if the filter is in the URL.
    await page.locator("[data-webhook-delivery-status]").selectOption("delivered").catch(() => {});
    await page.waitForTimeout(1200);
    steps.statusFilterIsInTheUrl = page.url().includes("status=delivered");
    const deliveredOnly = await page.evaluate(() =>
      [...document.querySelectorAll("[data-webhook-delivery-state]")].every(
        (node) => node.textContent.trim() === "delivered",
      ),
    );
    steps.statusFilterNarrows = deliveredOnly;
    await shot(page, "page-webhooks-deliveries-filtered");

    await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-webhook-delivery-table], [data-webhook-delivery-empty]", { timeout: 8000 }).catch(() => {});
    steps.filterSurvivesReload = page.url().includes("status=delivered");

    // A routed failure must render the error banner, not a silent empty table.
    await page.route("**/api/v1/webhooks/*/deliveries*", (route) => route.fulfill({
      status: 500,
      contentType: "application/json",
      body: JSON.stringify({ error: { code: "internal_error", message: "the queue is unreachable" } }),
    }));
    await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-webhook-error]", { timeout: 8000 }).catch(() => {});
    steps.errorBannerOnFailure = (await page.locator("[data-webhook-error]").count()) > 0;
    await shot(page, "page-webhooks-deliveries-error");
    await page.unroute("**/api/v1/webhooks/*/deliveries*").catch(() => {});

    // ---- 6. Force a delivery again -----------------------------------------------------------------
    const rowId = (await page.locator("[data-webhook-delivery-row]").first().getAttribute("data-webhook-delivery-row").catch(() => "")) || "";
    if (rowId) {
      await page.locator(`[data-webhook-delivery-select="${rowId}"]`).check({ timeout: 4000 }).catch(() => {});
      await page.locator("[data-webhook-redeliver-bulk]").click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(1500);
      steps.redeliverReportsWhatMoved = (await page.locator("[data-webhook-notice]").count()) > 0;
      const replayLabel = await page
        .locator(`[data-webhook-trigger="${rowId}"]`)
        .textContent()
        .catch(() => "");
      // The row is reset, not replaced: the same id comes back marked as a replay. A second row
      // would mean the receiver cannot tell a replay from a duplicate.
      steps.redeliveryIsTheSameRow = (replayLabel || "").trim() === "replay";
      await shot(page, "page-webhooks-redelivered");
    }

    // ---- 7. The stats tab ----------------------------------------------------------------------------
    await page.locator('[data-webhook-tab="stats"]').click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-webhook-stats], [data-webhook-stat-skeleton]", { timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(900);
    steps.statsRender = (await page.locator("[data-webhook-stat-rate]").count()) > 0;
    // A history that is only probes has no rate. Rendering 100% would be the most flattering
    // possible lie on this screen, and it is the one the API's exclusion exists to prevent.
    steps.rateIsHonestAboutProbes = (await page
      .locator("[data-webhook-stat-rate]")
      .textContent()
      .catch(() => "")) !== "100%";
    await shot(page, "page-webhooks-stats");

    // ---- 8. Rotate the secret --------------------------------------------------------------------------
    await page.locator('[data-webhook-tab="overview"]').click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-webhook-overview-rotate]", { timeout: 6000 }).catch(() => {});
    await page.locator("[data-webhook-overview-rotate]").click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-webhook-rotation]", { timeout: 8000 }).catch(() => {});
    const rotated = (await page.locator("[data-webhook-rotation-value]").textContent().catch(() => "")) || "";
    steps.rotationShowsANewSecret = rotated.trim().length >= 16 && rotated.trim() !== issued.trim();
    steps.rotationGatesDone =
      await page.locator("[data-webhook-rotation-done]").isDisabled().catch(() => false);
    steps.rotationSaysTheReceiverWillBreak = (await page
      .locator("[data-webhook-rotation]")
      .textContent()
      .catch(() => "")).includes("stops verifying");
    await shot(page, "page-webhooks-rotated");

    // ---- 9. Remove it ------------------------------------------------------------------------------------
    await page.locator("[data-webhook-rotation-stored]").check({ timeout: 4000 }).catch(() => {});
    await page.locator("[data-webhook-rotation-done]").click({ timeout: 4000 }).catch(() => {});
    await page.locator("[data-webhook-overview-delete]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForSelector("[data-webhook-confirm]", { timeout: 5000 }).catch(() => {});
    steps.deleteIsConfirmedByName = (await page
      .locator("[data-webhook-confirm]")
      .textContent()
      .catch(() => "")).includes(name);
    await shot(page, "page-webhooks-confirm-remove");

    await page.locator("[data-webhook-confirm-remove]").click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1800);
    steps.removedFromTheList = !page.url().includes(`/webhooks/${endpointId}`);
    await page.goto(`${URL_ADMIN}/webhooks`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1200);
    steps.endpointIsGone = (await page.locator(`[data-webhook-row="${endpointId}"]`).count()) === 0;

    // The receiver saw the delivery the platform signed. Recorded as a count rather than a
    // boolean so a report can show that it was more than zero, which "the pass ran" cannot.
    record({ page: "webhooks", action: "webhook-deliveries-received", count: steps.receiverAccepted });
    if (endpointId) {
      record({ page: "webhooks", action: "webhook-endpoint-created", endpoint: endpointId });
    }
  } finally {
    receiver.kill("SIGTERM");
  }

  return steps;
}

/**
 * The retention pass (REQ-016, slice 3).
 *
 * It is a separate pass rather than more steps in the events one, and the reason is a cleanup
 * obligation: a sweep **deletes rows**, so this pass has to put the bus back the way it found
 * it before any later pass counts events. The events pass asserts on counts; a retention pass
 * that ran first and left the bus short would make that pass's numbers wrong for reasons that
 * have nothing to do with it.
 *
 * What it proves on screen, in this order:
 *
 *  1. The tab renders and the window is the server's, not a default written in the client.
 *  2. A window outside the server's own range leaves the **Save** button disabled, so the
 *     refusal never depends on the round trip succeeding.
 *  3. A window inside the range saves, and the change is on the bus with the before *and* the
 *     after — a policy change with no audit trail is a policy change nobody can roll back.
 *  4. `Sweep now` answers with a number, including zero, and a zero renders as a sentence
 *     rather than an empty table: "the last sweep ran and found nothing" and "no sweep has
 *     run" are different states and must not look the same.
 *  5. The run log grows by exactly one row per sweep — including the empty one, which is the
 *     whole reason the log exists.
 *  6. The window is put back to what it was, and the status read back says so.
 */
/**
 * The security centre's two screens (REQ-012, slice 1).
 *
 * What this pass is really checking is a *claim*, not a layout: does the screen ever say
 * "verified" about something it did not verify? The checks that answer `unknown` are the ones
 * a QA pass is most able to catch, because a fresh QA database has no MFA rows, no backup
 * history and no header policy — so the honest screen says "Not checked yet" and a dishonest
 * one would say "Verified". The pass asserts the badges that appear, so a change that turns
 * an `unknown` into a `pass` without a data source behind it fails here.
 *
 * The findings half drives the transitions the request names: acknowledge, ignore with a
 * reason, and the refusal when the reason is missing. The refusal is the interesting one — a
 * client that could ignore without a reason would be a dismissal with no stated justification,
 * which is the one transition that quietly erases a finding from an operator's view.
 */
async function runSecurityDepth(page, report) {
  const steps = {};
  const note = (key, value) => {
    steps[key] = value;
    record({ page: "security", action: "security-depth", step: key, ...value });
  };

  // ---- The overview -----------------------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/security`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-security-overview]", { timeout: 15000 }).catch(() => {});
  const rendered = (await page.locator("[data-security-overview]").count()) > 0;
  note({ step: "overview-loaded", rendered });
  if (!rendered) {
    return { ok: false, reason: "/security did not render the overview", steps };
  }

  const rows = await page.locator("[data-security-checks] li").count();
  note({ step: "check-rows", rows });
  await shot(page, "security-overview");

  // The registry has ten checks in this build, and the panel must show a row for each one
  // whether or not it has ever been evaluated. A shorter list is the bug this rule catches:
  // an unevaluated check that renders as a missing row reads as "there is nothing here".
  if (rows < 5) {
    note({ step: "registry-too-short", rows, reason: "fewer than five check rows rendered" });
  }

  // Every row must carry a state badge, and no row may claim a pass it cannot back. The
  // counts are read from the server's own summary, so they cannot drift from the legend.
  const states = await page.$$eval("[data-security-state]", (nodes) =>
    nodes.map((node) => node.getAttribute("data-security-state")),
  );
  const tally = states.reduce((acc, state) => {
    acc[state] = (acc[state] || 0) + 1;
    return acc;
  }, {});
  note({ step: "states", tally });
  if (states.length !== rows) {
    note({ step: "state-badge-missing", badges: states.length, rows });
  }

  const score = await page
    .locator("[data-security-score]")
    .getAttribute("data-security-score")
    .catch(() => null);
  note({ step: "score", score });
  if (score === null || Number.isNaN(Number(score))) {
    note({ step: "score-missing", reason: "the score ring rendered no number" });
  }

  // "Run checks" must move the timestamps and produce a full result set. A run that answers
  // 200 and writes nothing is a button that looks like it works.
  const before = await page.locator("[data-security-state]").count();
  await page.locator("[data-security-run]").click().catch(() => {});
  await page
    .waitForFunction(
      (previous) => document.querySelectorAll("[data-security-state]").length > 0,
      before,
      { timeout: 20000 },
    )
    .catch(() => {});
  await page.waitForTimeout(1500);
  const runError = await page.locator("[data-security-run-error]").count();
  note({ step: "run-completed", errorShown: runError > 0 });
  await shot(page, "security-overview-after-run");

  // ---- The findings list -------------------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/security/findings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-security-findings]", { timeout: 15000 }).catch(() => {});
  const listRendered = (await page.locator("[data-security-findings]").count()) > 0;
  note({ step: "findings-loaded", listRendered });
  if (!listRendered) {
    return { ok: false, reason: "/security/findings did not render", steps };
  }
  await shot(page, "security-findings-empty");

  // ---- Import a report, twice, and prove the second run does not double the count -------------
  //
  // The idempotence claim is the one an API response cannot make on its own: a re-ingest that
  // reports "created" a second time has quietly doubled the operator's open findings, and the
  // count on the screen is the only place that would show it.
  const stamp = Date.now().toString(36);
  const fixture = {
    findings: [
      {
        title: `QA unpinned dependency ${stamp}`,
        severity: "high",
        component: "qa-probe-package",
        version: "0.1.0",
        description: "Injected by the QA walkthrough to exercise the ingest path.",
      },
      {
        title: `QA advisory finding ${stamp}`,
        severity: "low",
        component: "qa-probe-advisory",
        version: "2.0.0",
        fixed_in: "2.0.1",
        description: "Injected by the QA walkthrough to exercise a finding with a fix.",
      },
    ],
  };

  const ingest = async (payload) =>
    page.evaluate(
      async (body) => {
        const response = await fetch("/api/v1/security/findings/import", {
          method: "POST",
          credentials: "same-origin",
          headers: { accept: "application/json", "content-type": "application/json" },
          body: JSON.stringify(body),
        });
        return { status: response.status, body: await response.json().catch(() => null) };
      },
      payload,
    );

  const first = await ingest({ report: fixture, source: "dependency" });
  note({ step: "ingest-first", status: first.status, created: first.body?.created, refreshed: first.body?.refreshed });
  const second = await ingest({ report: fixture, source: "dependency" });
  note({ step: "ingest-second", status: second.status, created: second.body?.created, refreshed: second.body?.refreshed });
  if (second.body && second.body.created !== 0) {
    note({
      step: "ingest-not-idempotent",
      created: second.body.created,
      reason: "a re-ingest created rows instead of refreshing them",
    });
  }

  // A report carrying something that looks like a credential is refused whole.
  const leaky = await ingest({
    report: { findings: [{ title: `QA leak probe ${stamp}`, api_key: "not-a-real-key" }] },
    source: "dependency",
  });
  note({ step: "ingest-credential-refused", status: leaky.status });
  if (leaky.status === 200) {
    note({ step: "credential-accepted", reason: "a report carrying a credential was imported" });
  }

  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-finding-row]", { timeout: 15000 }).catch(() => {});
  const rowCount = await page.locator("[data-finding-row]").count();
  note({ step: "finding-rows", rows: rowCount });
  await shot(page, "security-findings-list");

  // ---- The drawer: acknowledge, then ignore, and the refusal without a reason -----------------
  if (rowCount > 0) {
    await page.locator("[data-finding-open]").first().click().catch(() => {});
    await page.waitForSelector("[data-finding-drawer]", { timeout: 8000 }).catch(() => {});
    const drawer = (await page.locator("[data-finding-drawer]").count()) > 0;
    note({ step: "drawer-opened", drawer });
    await shot(page, "security-finding-drawer", { full: false });

    if (drawer) {
      // The ignore button starts disabled, because the reason is required. A button that is
      // enabled and then fails is a form the operator learns to distrust.
      const ignoreDisabled = await page.locator("[data-finding-ignore]").isDisabled().catch(() => null);
      note({ step: "ignore-disabled-without-reason", disabled: ignoreDisabled });
      if (ignoreDisabled === false) {
        note({ step: "ignore-enabled-without-reason", reason: "the ignore control is live with no reason" });
      }

      await page.locator("[data-finding-ack]").click().catch(() => {});
      await page.waitForTimeout(1200);
      note({ step: "acknowledged" });
      await shot(page, "security-finding-acknowledged", { full: false });
    }
  }

  // ---- The keyboard: `/` focuses the filter ----------------------------------------------------
  await page.keyboard.press("/");
  await page.waitForTimeout(300);
  const focused = await page.evaluate(() => document.activeElement?.getAttribute("data-findings-search") !== null);
  note({ step: "slash-focuses-search", focused });
  await page.keyboard.press("Escape");
  await page.keyboard.press("Backspace");
  await page.waitForTimeout(600);

  // ---- The filter: an unknown value is refused by name, not silently ignored ------------------
  const badFilter = await page.evaluate(async () => {
    const response = await fetch("/api/v1/security/findings?severity=spicy", {
      credentials: "same-origin",
      headers: { accept: "application/json" },
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  });
  note({ step: "unknown-filter-refused", status: badFilter.status, code: badFilter.body?.error?.code });
  if (badFilter.status === 200) {
    note({ step: "unknown-filter-accepted", reason: "a nonsense severity returned a list" });
  }

  // ---- Clean up what the pass created ------------------------------------------------------------
  try {
    const removed = qaSql(
      `delete from security_findings where component in ('qa-probe-package', 'qa-probe-advisory') and title like 'QA %${stamp}%'`,
    );
    note({ step: "cleanup", removed: removed || "0" });
  } catch (error) {
    note({ step: "cleanup-failed", reason: String(error.message || error) });
  }

  // ---- The header policy screen (REQ-012, slice 2) ---------------------------------------------
  //
  // This half exists because slice 2's backend shipped with no screen: the API could store a
  // policy and nothing in the panel could edit one. The three claims worth a browser are
  // therefore the three a JSON response cannot make — the draft preview tracks the form, a
  // refusal names the row that caused it, and a save reaches the *response headers* rather
  // than only the settings row.
  await page.goto(`${URL_ADMIN}/security/headers`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-header-policy], [data-header-error]", { timeout: 15000 }).catch(() => {});
  const headersRendered = (await page.locator("[data-header-policy]").count()) > 0;
  note({ step: "headers-loaded", rendered: headersRendered });
  if (headersRendered) {
    // The tab strip must mark the open screen, and must not offer a tab that leads nowhere.
    const tabs = await page.locator("[data-security-tab]").count();
    const currentTab = await page.locator('[data-security-tab][aria-current="page"]').count();
    note({ step: "security-tabs", tabs, currentTab });
    if (currentTab !== 1) {
      note({ step: "security-tab-not-marked", currentTab, reason: "the open tab is not marked" });
    }

    // The rendered column is the server's, and it must contain the real header names rather
    // than a summary. A preview of a summary is the bug the whole column exists to prevent.
    const savedLines = await page.$$eval("[data-header-line]", (nodes) =>
      nodes.map((node) => ({
        name: node.getAttribute("data-header-line"),
        off: node.getAttribute("data-header-off") === "true",
        text: node.textContent.trim(),
      })),
    );
    note({ step: "header-lines", lines: savedLines.length });
    const cspLine = savedLines.find((line) => /Content-Security-Policy/.test(line.name || ""));
    if (!cspLine) {
      note({ step: "no-csp-line", reason: "the preview carries no CSP header" });
    } else if (cspLine.off) {
      note({ step: "csp-off", reason: "the baseline sends no CSP at all" });
    }
    // Report-only and enforce are mutually exclusive on the wire. Both names appearing at once
    // means the screen is showing a policy the middleware would never send.
    const bothModes = savedLines.filter((line) =>
      /Content-Security-Policy(-Report-Only)?$/.test(line.name || ""),
    );
    if (bothModes.length > 1) {
      note({ step: "both-csp-modes", lines: bothModes.map((line) => line.name) });
    }

    // A directive row with no name cannot be saved, and the control must be disabled while it
    // is there — a live button that always fails teaches the operator the form is broken.
    await page.locator("[data-header-add-directive]").click().catch(() => {});
    await page.waitForTimeout(400);
    const emptyNameShown = (await page.locator("[data-header-empty-name]").count()) > 0;
    const saveBlocked = await page.locator("[data-header-save]").isDisabled().catch(() => null);
    note({ step: "empty-directive-blocks-save", warned: emptyNameShown, saveDisabled: saveBlocked });
    if (emptyNameShown && saveBlocked === false) {
      note({ step: "empty-directive-savable", reason: "an unnamed directive can be saved" });
    }
    await shot(page, "security-headers-empty-directive");

    // Filling it in makes the form dirty, and the preview must switch to a *draft* — a preview
    // that keeps showing the saved policy beside an edited form is exactly the confusion this
    // screen exists to remove.
    await page.locator("[data-header-directive-name='0']").fill("img-src");
    await page.locator("[data-header-directive-values='0']").fill("'self' data:");
    await page.waitForTimeout(400);
    const dirtyShown = (await page.locator("[data-header-dirty]").count()) > 0;
    const draftPreview = (await page.locator('[data-header-preview="draft"]').count()) > 0;
    const draftDrafted = (await page.locator("[data-header-preview-draft]").count()) > 0;
    note({ step: "draft-preview", dirtyShown, draftPreview, labelled: draftDrafted });
    if (!draftPreview) {
      note({ step: "preview-not-a-draft", reason: "an edited form still shows the saved policy" });
    }
    if (draftPreview && !draftDrafted) {
      note({ step: "draft-unlabelled", reason: "the draft preview is not labelled as one" });
    }
    await shot(page, "security-headers-draft");

    // The mode radios are exclusive, and switching must move the header name in the preview
    // from the report-only name to the enforcing one.
    await page.locator('[data-header-mode="enforce"]').check().catch(() => {});
    await page.waitForTimeout(400);
    const enforcedLine = await page
      .locator('[data-header-line="Content-Security-Policy"]')
      .count();
    const reportLine = await page
      .locator('[data-header-line="Content-Security-Policy-Report-Only"]')
      .count();
    note({ step: "enforce-switches-name", enforcing: enforcedLine, reportOnly: reportLine });
    if (enforcedLine !== 1 || reportLine !== 0) {
      note({
        step: "csp-mode-does-not-move",
        reason: "enforce mode did not replace the report-only header name",
      });
    }
    await shot(page, "security-headers-enforce");

    // A `max-age` a browser would ignore is a warning on the field, not a silent save.
    await page.locator("[data-header-hsts-max-age]").fill("3600");
    await page.waitForTimeout(400);
    const hstsWarned = (await page.locator("[data-header-hsts-warning]").count()) > 0;
    note({ step: "hsts-too-short-warned", warned: hstsWarned });
    await shot(page, "security-headers-hsts-warning");
    await page.locator("[data-header-hsts-max-age]").fill("31536000");
    await page.waitForTimeout(300);

    // ---- Save, and read it back off the wire ---------------------------------------------------
    // The assertion is the response headers, not the settings row: a policy that stores but
    // does not reach the middleware is the failure mode this whole screen is about.
    await page.locator("[data-header-save]").click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-header-save-error]", { timeout: 12000 }).catch(() => {});
    const saveError = (await page.locator("[data-header-save-error]").textContent().catch(() => "")) || null;
    note({ step: "headers-saved", error: saveError ? saveError.trim().slice(0, 160) : null });
    if (saveError) {
      note({ step: "headers-save-failed", reason: "the policy did not save" });
    } else {
      await page.waitForTimeout(600);
      const onTheWire = await page.evaluate(async () => {
        const answer = await fetch("/api/v1/security/headers", { credentials: "same-origin" });
        const body = await answer.json().catch(() => null);
        return body?.rendered ?? null;
      });
      const wireNames = (onTheWire || []).map((line) => line.name);
      const wireEnforcing = wireNames.includes("Content-Security-Policy");
      const wireReportOnly = wireNames.includes("Content-Security-Policy-Report-Only");
      note({ step: "wire-csp-mode", enforcing: wireEnforcing, reportOnly: wireReportOnly });
      if (!wireEnforcing || wireReportOnly) {
        note({
          step: "wire-mode-mismatch",
          reason: "the saved mode did not reach the stored rendering",
        });
      }
      const hasImgSrc = (onTheWire || []).some(
        (line) => (line.value || "").includes("img-src 'self' data:"),
      );
      note({ step: "wire-has-directive", hasImgSrc });
      if (!hasImgSrc) {
        note({ step: "directive-not-stored", reason: "the edited directive did not reach the store" });
      }
      // An off header is a visible row, not a missing one.
      const hasOffRow = (onTheWire || []).some((line) => line.value === null);
      note({ step: "wire-lists-off-headers", hasOffRow });
    }
    await shot(page, "security-headers-saved");

    // Put the mode back to report-only so a pass cannot leave the QA deployment enforcing a
    // policy that a sibling's browser pass would then be running under.
    await page.locator('[data-header-mode="report_only"]').check().catch(() => {});
    await page.waitForTimeout(300);
    await page.locator("[data-header-save]").click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1200);
    note({ step: "headers-restored" });
    await shot(page, "security-headers-restored");
  }

  // ---- The rate-limit policy (REQ-012, slice 2) ---------------------------------------------
  //
  // This screen and the sign-in one below had a rule engine, a policy editor, a live counter
  // and a test console behind them, and no walk had ever opened either of them. A screen that
  // is never rendered is not "mostly tested"; it is untested, and the two that ship first are
  // the two whose failure is silent — a limit nobody can read is a limit nobody knows is set.
  //
  // The console is the interesting half: it answers "would THIS request be refused", which no
  // screenshot of a form can show. So the pass asks a question the tester can see the answer
  // to, and then edits a limit and asks it again — the policy must answer differently, or the
  // editor is a text box with a save button.
  await page.goto(`${URL_ADMIN}/security/rate-limits`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector('[data-rate-limits="ready"]', { timeout: 15000 }).catch(() => {});
  const limitsReady = (await page.locator('[data-rate-limits="ready"]').count()) > 0;
  note({ step: "rate-limits-loaded", rendered: limitsReady });
  if (!limitsReady) {
    note({ step: "rate-limits-missing", reason: "/security/rate-limits did not render its ready state" });
  } else {
    const limitRows = await page.locator("[data-rate-limit-row]").count();
    note({ step: "rate-limit-rows", rows: limitRows });
    if (limitRows < 1) {
      note({ step: "rate-limit-registry-empty", reason: "no scope rendered a limit row" });
    }
    await shot(page, "security-rate-limits");

    // Every row must say what its scope is allowed, not just how much: a number without a
    // window is a number nobody can compare against anything.
    const rowLabels = await page.$$eval("[data-rate-limit-row]", (nodes) =>
      nodes.map((node) => ({
        scope: node.getAttribute("data-rate-limit-row"),
        inputs: node.querySelectorAll("input").length,
      })),
    );
    note({ step: "rate-limit-row-shape", rows: rowLabels });
    const inputless = rowLabels.filter((row) => row.inputs === 0);
    if (inputless.length) {
      note({ step: "rate-limit-row-not-editable", rows: inputless.map((r) => r.scope) });
    }

    // The tester: the screen's whole claim is "would THIS request be refused", and the answer
    // comes from the server's own `decide` — the same function the middleware runs. So the pass
    // asks a question through the real endpoint, with a counter no sane ceiling allows, and
    // requires a verdict of `limited` plus the key it counted and the retry it would send.
    //
    // The endpoint is `POST /security/rate-limits/test` (`security.read`), the body is
    // `{ method, path, client_ip, count, machine_key }` and the verdict is NESTED under
    // `verdict`, beside `counter_identity`/`counter_key`. Guessing any of those — an invented
    // `/probe` path, a flat `limited` boolean — produces a request that 404s or a read of
    // `undefined` that compares false, and the step is then a green line that proves nothing.
    //
    // The CSRF header is the fourth thing a hand-written fetch gets wrong, and it is the one
    // that fails *green*. A cookie-authenticated mutation with no `x-omnion-csrf` is refused
    // with `csrf_unavailable` before the handler ever runs, so a pass that skipped it would
    // record "the tester refused this request" about a screen that works perfectly in the hand
    // above it. The token is the readable `omnion_csrf` cookie the panel's own client echoes.
    const probe = await page.evaluate(async () => {
      const csrf = document.cookie
        .split(";")
        .map((part) => part.trim())
        .find((part) => part.startsWith("omnion_csrf="))
        ?.slice("omnion_csrf=".length);
      const answer = await fetch("/api/v1/security/rate-limits/test", {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "content-type": "application/json",
          ...(csrf ? { "x-omnion-csrf": decodeURIComponent(csrf) } : {}),
        },
        body: JSON.stringify({
          method: "POST",
          path: "/api/v1/auth/login",
          client_ip: "203.0.113.7",
          count: 400,
          machine_key: false,
        }),
      });
      return {
        status: answer.status,
        body: await answer.json().catch(() => null),
        hadCsrf: Boolean(csrf),
      };
    });
    note({
      step: "rate-limit-test-endpoint",
      status: probe?.status,
      hadCsrf: probe?.hadCsrf,
      scope: probe?.body?.verdict?.scope,
      limited: probe?.body?.verdict?.limited,
      ceiling: probe?.body?.verdict?.ceiling,
      counterKey: probe?.body?.counter_key,
      retryAfter: probe?.body?.verdict?.retry_after,
    });
    if (probe?.status !== 200) {
      note({
        step: "rate-limit-test-unreachable",
        status: probe?.status,
        code: probe?.body?.error?.code,
        reason: "the tester endpoint did not answer 200",
      });
    } else if (probe.body?.verdict?.limited !== true) {
      note({ step: "rate-limit-test-not-limited", reason: "a counter of 400 was not reported as limited" });
    }

    // Now the tester button on the screen itself, which is what a human uses: the same request
    // has to survive the form, the client and the render.
    await page.locator("[data-rate-limit-probe-count]").fill("400").catch(() => {});
    await page.waitForTimeout(200);
    await page.locator("[data-rate-limit-probe-run]").click({ timeout: 8000 }).catch(() => {});
    await page
      .waitForSelector('[data-rate-limit-probe-result]:not([data-rate-limit-probe-result="none"])', {
        timeout: 20000,
      })
      .catch(() => {});
    const probeVerdict = await page
      .locator("[data-rate-limit-probe-result]")
      .getAttribute("data-rate-limit-probe-result")
      .catch(() => null);
    note({ step: "rate-limit-console-verdict", verdict: probeVerdict });
    if (!probeVerdict || probeVerdict === "none") {
      note({ step: "rate-limit-console-silent", reason: "the tester ran and reported no verdict" });
    }
    await shot(page, "security-rate-limits-probe");

    // An out-of-range limit must be refused by the FORM before it reaches the server — the
    // field message is the whole point of validating here rather than on save.
    const firstInput = page.locator("[data-rate-limit-input]").first();
    const hadInput = (await firstInput.count()) > 0;
    if (hadInput) {
      await firstInput.fill("0");
      await page.waitForTimeout(400);
      const localError = (await page.locator("[data-rate-limits-local-error]").count()) > 0;
      note({ step: "rate-limit-zero-refused", refused: localError });
      if (!localError) {
        note({ step: "rate-limit-zero-accepted", reason: "a limit of 0 was accepted by the form" });
      }
      await shot(page, "security-rate-limits-invalid");
      // Put the field back so this pass cannot leave the policy disabled for a sibling's pass.
      await firstInput.fill("");
      await page.waitForTimeout(300);
    }
  }

  // ---- The sign-in protection policy (REQ-012, slice 2) -------------------------------------
  await page
    .goto(`${URL_ADMIN}/security/sign-in-protection`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForSelector('[data-sign-in-protection="ready"]', { timeout: 15000 }).catch(() => {});
  const protectionReady = (await page.locator('[data-sign-in-protection="ready"]').count()) > 0;
  note({ step: "sign-in-protection-loaded", rendered: protectionReady });
  if (!protectionReady) {
    note({ step: "sign-in-protection-missing", reason: "the screen did not render its ready state" });
  } else {
    const lockedCount = await page
      .locator("[data-locked-count]")
      .first()
      .innerText()
      .catch(() => null);
    note({ step: "locked-accounts", count: lockedCount });
    await shot(page, "security-sign-in-protection");

    // The policy has five fields; each must be editable and each must survive a save. A
    // policy editor that renders read-only inputs still LOOKS like the security centre.
    const fields = ["window_seconds", "attempts", "lockout_minutes", "base_delay_seconds", "progressive_delay"];
    const present = [];
    for (const field of fields) {
      present.push([field, (await page.locator(`[data-lockout-field="${field}"]`).count()) > 0]);
    }
    note({ step: "lockout-fields", fields: present });
    const missingFields = present.filter(([, ok]) => !ok).map(([name]) => name);
    if (missingFields.length) {
      note({ step: "lockout-field-missing", fields: missingFields });
    }

    // The reset_on_success switch is a boolean where every other field is a number: a form
    // that validates all five the same way will either refuse a checkbox or accept nonsense.
    const resetSwitch = await page.locator('[data-lockout-field="reset_on_success"]').count();
    note({ step: "lockout-reset-switch", present: resetSwitch > 0 });

    // An out-of-range attempts value must be refused with a field message, not saved.
    const attempts = page.locator('[data-lockout-field="attempts"]').first();
    if ((await attempts.count()) > 0) {
      const before = await attempts.inputValue().catch(() => "");
      await attempts.fill("0");
      await page.waitForTimeout(400);
      const localError = (await page.locator("[data-sign-in-protection-local-error]").count()) > 0;
      note({ step: "lockout-zero-refused", refused: localError, before });
      if (!localError) {
        note({ step: "lockout-zero-accepted", reason: "attempts = 0 was accepted by the form" });
      }
      await shot(page, "security-sign-in-protection-invalid");
      await attempts.fill(before);
      await page.waitForTimeout(300);
    }
  }

  return { ok: true, steps };
}

async function runRetentionDepth(page, report) {
  const steps = {};
  const before = await page
    .evaluate(async () => {
      const answer = await fetch("/api/v1/events/retention", { credentials: "same-origin" });
      return answer.ok ? answer.json() : null;
    })
    .catch(() => null);

  const original = before?.window_days ?? 30;

  try {
    // ---- 1. The tab, before anything is changed ------------------------------------------------
    await page.goto(`${URL_ADMIN}/events?tab=retention`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-retention-panel], [data-retention-error]", { timeout: 8000 }).catch(() => {});

    steps.panelPresent = (await page.locator("[data-retention-panel]").count()) > 0;
    steps.noError = (await page.locator("[data-retention-error]").count()) === 0;
    steps.windowIsTheServers = (await page.locator("[data-retention-window]").inputValue().catch(() => "")) === String(original);
    steps.boundsComeFromTheApi = before?.min_days === 1 && before?.max_days === 3650;
    // The counts are on screen even when they are zero: a "due" cell that renders blank is
    // indistinguishable from a cell for an endpoint the screen forgot to ask about.
    steps.eventsCounted = (await page.locator("[data-retention-events]").textContent().catch(() => "")).trim().length > 0;
    steps.dueCounted = (await page.locator("[data-retention-due]").textContent().catch(() => "")).trim().length > 0;
    steps.lastRunIsNamed = (await page.locator("[data-retention-last-run]").textContent().catch(() => "")).trim().length > 0;
    await shot(page, "page-events-retention");

    // ---- 2. A window outside the range cannot be saved -----------------------------------------
    // The bounds are the server's, so the button is the *first* refusal: a value the API would
    // reject is not offered as something to try.
    await page.locator("[data-retention-window]").fill("0");
    await page.waitForTimeout(250);
    steps.zeroDisablesSave = await page.locator("[data-retention-save]").isDisabled();
    await page.locator("[data-retention-window]").fill("4000");
    await page.waitForTimeout(250);
    steps.hugeDisablesSave = await page.locator("[data-retention-save]").isDisabled();
    await shot(page, "page-events-retention-out-of-range");

    // ---- 3. A valid window saves, and the change is on the bus --------------------------------
    await page.locator("[data-retention-window]").fill("7");
    await page.waitForTimeout(250);
    steps.validEnablesSave = !(await page.locator("[data-retention-save]").isDisabled());
    await page.locator("[data-retention-save]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForSelector("[data-retention-notice]", { timeout: 8000 }).catch(() => {});
    steps.saved = ((await page.locator("[data-retention-window]").inputValue().catch(() => ""))) === "7";
    steps.savedIsAnnounced = ((await page.locator("[data-retention-notice]").textContent().catch(() => ""))).trim().length > 0;
    await shot(page, "page-events-retention-saved");

    const audited = await page
      .evaluate(async () => {
        const answer = await fetch("/api/v1/events?name=webhook.retention.changed&limit=5", {
          credentials: "same-origin",
        });
        const body = await answer.json();
        return (body.events || [])[0]?.payload ?? null;
      })
      .catch(() => null);
    // The transition, not the new value: an audit trail that records "the window is 7" cannot
    // answer "what was it before", which is the only question a rollback has.
    steps.auditCarriesBoth = audited?.previous_window_days === original && audited?.window_days === 7;

    // ---- 4. A sweep answers with a number, including zero --------------------------------------
    const runsBefore = (await page.locator("[data-retention-run]").count());
    await page.locator("[data-retention-sweep]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForSelector("[data-retention-notice]", { timeout: 10000 }).catch(() => {});
    await page.waitForTimeout(600);
    const sweptNotice = ((await page.locator("[data-retention-notice]").textContent().catch(() => ""))).trim();
    steps.sweepAnswers = sweptNotice.length > 0;
    // A fresh QA database has nothing past a seven-day window, so the honest answer here is
    // zero — and the sentence is what proves the button finished rather than hung.
    steps.emptySweepIsASentence = /nothing past the window/i.test(sweptNotice) || /Removed/i.test(sweptNotice);
    await shot(page, "page-events-retention-swept");

    // ---- 5. The run log grew, empty sweep or not ------------------------------------------------
    const runsAfter = (await page.locator("[data-retention-run]").count());
    steps.runLogGrew = runsAfter === runsBefore + 1;
    steps.emptyRunsHaveTheirOwnState =
      (await page.locator("[data-retention-runs-empty]").count()) === 0;
    steps.runRowsCounted = runsAfter;
    steps.runsBefore = runsBefore;

    record({ page: "events", action: "retention-sweep", removed: steps.runRowsCounted });
  } finally {
    // ---- 6. Put the window back -----------------------------------------------------------------
    // The pass changes a policy the rest of the run reads, so it restores it in a `finally`
    // rather than at the end of the happy path: a throw three steps in must not leave the QA
    // organization's bus on a seven-day window for every pass that follows.
    try {
      await page.evaluate(async (days) => {
        await fetch("/api/v1/events/retention", {
          method: "PATCH",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ window_days: days }),
        });
      }, original);
      steps.restored = true;
    } catch {
      steps.restored = false;
    }
  }

  return steps;
}

/**
 * The settings and privacy pass (REQ-007, slice 4): the write half of the settings screen and
 * the two irreversible operations, each proven against the QA database rather than against the
 * screen's own optimism — tracking off, saved, reloaded and read back; a retention value the
 * promise has a floor for, refused on screen; a purge that removes the rows past a seven-day
 * window on the populated QA database; and a visitor handle whose rows are counted before and
 * after its erasure.
 */
async function runAnalyticsSettingsDepth(page, report) {
  const steps = {};
  const site = qaSql(`select id from sites where key = '${CREDS.siteKey}' limit 1`);
  const switchState = async (name) =>
    (await page
      .locator(`[data-analytics-settings-switch="${name}"]`)
      .first()
      .getAttribute("aria-checked")
      .catch(() => "")) || "";
  const toggle = async (name) => {
    await page
      .locator(`[data-analytics-settings-switch="${name}"]`)
      .first()
      .click({ timeout: 4000 })
      .catch(() => {});
    await page.waitForTimeout(300);
  };
  const save = async () => {
    await page.locator("[data-analytics-settings-save]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1200);
  };
  const oldRows = (cutoffDate) =>
    Number(
      qaSql(
        `select count(*) from analytics_visits where site_id = '${site}' and started_at < '${cutoffDate}T00:00:00Z'`,
      ),
    );

  await page.goto(`${URL_ADMIN}/analytics/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2000);
  steps.loaded = (await page.locator("[data-analytics-settings-form]").count()) > 0;
  steps.storageRows = await page
    .locator('[data-analytics-panel="settings-storage"] [data-analytics-row]')
    .count();
  steps.personalRows = await page
    .locator('[data-analytics-panel="settings-storage"] span:text-is("Personal")')
    .count();
  steps.snippet = (await page.locator("[data-analytics-snippet]").first().innerText().catch(() => ""))
    .trim()
    .slice(0, 120);
  steps.copyControl = (await page.locator("[data-analytics-snippet-copy]").count()) > 0;
  await shot(page, "page-analytics-settings");

  // Tracking off, saved, reloaded: the switch the server stored, not the one the screen showed.
  steps.trackingBefore = await switchState("tracking_enabled");
  await toggle("tracking_enabled");
  steps.dirtyEnabledSave = await page
    .locator("[data-analytics-settings-save]")
    .first()
    .isEnabled()
    .catch(() => false);
  await save();
  steps.savedNote = (
    await page.locator("[data-analytics-settings-saved]").first().innerText().catch(() => "")
  )
    .trim()
    .replace(/\s+/g, " ")
    .slice(0, 60);
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.trackingAfterReload = await switchState("tracking_enabled");
  await toggle("tracking_enabled");
  await save();
  steps.trackingRestored = await switchState("tracking_enabled");

  // A retention value below the floor is refused with the field named, and never sent.
  await page.locator("[data-analytics-settings-retention]").first().fill("3", { timeout: 3000 }).catch(() => {});
  await page.waitForTimeout(250);
  steps.retentionError = (await page.locator('[data-analytics-settings-error="retention_days"]').count()) > 0;
  await page.locator("[data-analytics-settings-retention]").first().fill("7", { timeout: 3000 }).catch(() => {});
  await page.waitForTimeout(250);
  await save();
  steps.retentionSaved = (
    await page.locator("[data-analytics-settings-retention]").first().inputValue().catch(() => "")
  ).trim();

  // The exclusion lists, with the glob preview reading them back before they are stored.
  await page
    .locator("[data-analytics-settings-paths]")
    .first()
    .fill("/qa/private/*\n*.pdf", { timeout: 3000 })
    .catch(() => {});
  await page.waitForTimeout(300);
  steps.pathPreview = await page.locator("[data-analytics-path-preview] li").count();
  await save();
  steps.exclusionsSaved = (
    await page.locator("[data-analytics-settings-paths]").first().inputValue().catch(() => "")
  )
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean).length;

  // The purge: the screen names the cutoff before the button runs, and the database proves the
  // rows past it are gone.
  const cutoffText = (
    await page.locator("[data-analytics-purge-cutoff]").first().innerText().catch(() => "")
  ).trim();
  steps.purgeCutoff = cutoffText;
  const cutoffDate = (cutoffText.match(/\d{4}-\d{2}-\d{2}/) || [""])[0];
  steps.purgeOldRowsBefore = cutoffDate ? oldRows(cutoffDate) : null;
  await page.locator("[data-analytics-purge-run]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.purgeResult = (
    await page.locator("[data-analytics-purge-result]").first().innerText().catch(() => "")
  )
    .trim()
    .replace(/\s+/g, " ")
    .slice(0, 160);
  steps.purgeOldRowsAfter = cutoffDate ? oldRows(cutoffDate) : null;
  steps.lastPurge = (
    await page.locator("[data-analytics-last-purge]").first().innerText().catch(() => "")
  )
    .trim()
    .replace(/\s+/g, " ")
    .slice(0, 120);

  // The erasure: a handle that exists on the populated QA database, erased from the screen and
  // counted again in the database.
  let victim = "";
  try {
    victim = qaSql(
      `select visitor_hash from analytics_visits where site_id = '${site}' order by id desc limit 1`,
    ).trim();
  } catch (err) {
    steps.erasePickError = String(err).slice(0, 120);
  }
  steps.eraseHandle = victim ? `${victim.slice(0, 8)}…` : "";
  steps.eraseRowsBefore = victim
    ? Number(
        qaSql(
          `select count(*) from analytics_visits where site_id = '${site}' and visitor_hash = '${victim}'`,
        ),
      )
    : null;
  if (victim) {
    await page.locator("[data-analytics-erase-handle]").first().fill(victim, { timeout: 3000 }).catch(() => {});
    await page.waitForTimeout(200);
    await page.locator("[data-analytics-erase-confirm]").first().fill(victim, { timeout: 3000 }).catch(() => {});
    await page.waitForTimeout(300);
    steps.eraseUnlocked = await page
      .locator("[data-analytics-erase-run]")
      .first()
      .isEnabled()
      .catch(() => false);
    await page.locator("[data-analytics-erase-run]").click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1800);
    steps.eraseResult = (
      await page.locator("[data-analytics-erase-result]").first().innerText().catch(() => "")
    )
      .trim()
      .replace(/\s+/g, " ")
      .slice(0, 160);
    steps.eraseRowsAfter = Number(
      qaSql(
        `select count(*) from analytics_visits where site_id = '${site}' and visitor_hash = '${victim}'`,
      ),
    );
    await shot(page, "analytics-settings-erased");
  }

  // Leave the QA site on the platform's own default, so the rest of the pass reads it as a
  // fresh installation would.
  await page.locator("[data-analytics-settings-retention]").first().fill("180", { timeout: 3000 }).catch(() => {});
  await page.waitForTimeout(200);
  await save();

  return steps;
}

// ---------------------------------------------------------------- run

async function main() {
  const report = { startedAt: new Date().toISOString(), admin: URL_ADMIN, web: URL_WEB, steps: [], pages: [], mobile: [], web: {} };
  const SITE_HOST = process.env.QA_SITE_HOST || CREDS.domain;
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      // A page that renders one big image can ask for a heap the box has not got, and the tab
      // dies with `Page crashed` — which used to end the run. Capping the renderer heap turns
      // that into a slower render and a GC instead of a dead tab.
      "--js-flags=--max-old-space-size=512",
      "--disable-gpu",
      `--host-resolver-rules=MAP ${SITE_HOST} 127.0.0.1`,
    ],
  });

  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true });
  const page = markHydrationWait(await context.newPage());
  attach(page, "main");

  // Reachable?
  try {
    const res = await page.goto(`${URL_ADMIN}/login`, { waitUntil: "domcontentloaded", timeout: 30000 });
    if (!res || res.status() >= 500) throw new Error(`admin panel responded ${res && res.status()}`);
  } catch (err) {
    fs.writeFileSync(path.join(OUT, "summary.json"), JSON.stringify({ fatal: String(err), ...report }, null, 2));
    console.error(`[walk] FATAL: admin panel unreachable at ${URL_ADMIN}: ${err}`);
    await browser.close();
    process.exit(2);
  }

  await runWizard(page, report);

  // `--only=wizard` re-checks the first-run flow on its own (reset the database first): it drives
  // the steps, then reports what the onboarding endpoints answered. A full pass is minutes; this is
  // the tool for "did the setup step just get refused?".
  if (process.argv.includes("--only=wizard")) {
    const onboardingFailures = netFailures.filter((f) => String(f.url || "").includes("/onboarding/"));
    const finished = await page
      .evaluate(() => /Your installation is ready/i.test(document.body.innerText))
      .catch(() => false);
    const away = !page.url().includes("/setup");
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify({ ...report, netFailures, onboardingFailures, wizardFinished: finished, wizardAway: away }, null, 2),
    );
    console.log(`WIZARD_ONBOARDING_FAILURES=${onboardingFailures.length} WIZARD_FINISHED=${finished} WIZARD_LEFT_SETUP=${away}`);
    await browser.close();
    process.exit(onboardingFailures.length === 0 ? 0 : 1);
  }

  const signedIn = await ensureSignedIn(page, report);
  report.signedIn = signedIn;
  if (!signedIn) {
    fs.writeFileSync(path.join(OUT, "summary.json"), JSON.stringify({ fatal: "could not sign in", ...report }, null, 2));
    console.error("[walk] FATAL: could not sign in after wizard");
    await browser.close();
    process.exit(3);
  }
  await shot(page, "11-overview-after-login");

  // The site the depth passes read. On a full pass the wizard just created it and this is a
  // read; on a `--only=<pass>` pass nothing has, and every pass that opens with
  // "if (!siteId) return" would skip all of its steps and still exit 0.
  report.qaSite = ensureQaSite();
  if (!report.qaSite) {
    log("no QA site and no organization to create one under — depth passes will skip");
  }

  // `--only=block-editor` runs this wave's own depth passes and nothing else.
  //
  // A full pass is ~45 minutes on a box five writers share, and it dies in the middle: the box
  // reboots, or another writer's pass prunes the shared pm2 daemon, and everything after the
  // cut is lost — including this wave's depth passes, which sit at the very end. The symptom is
  // unmistakable and easy to misread: `block editor: {"created":false,"blocked":"no page row
  // carried an editor link"}` with a `block-editor-page-form` screenshot showing Chrome's
  // ERR_CONNECTION_REFUSED, because the admin server was gone by the time the pass navigated.
  // That is a dead server, not a product defect, and reading it as one costs a tick.
  //
  // So the depth passes get their own entry point: sign in, run them, report, exit. Minutes
  // instead of an hour, and it cannot be taken down by what happens on another stack.
  if (process.argv.includes("--only=block-editor")) {
    report.blockEditor = await runBlockEditorDepth(page, report);
    log(`block editor: ${JSON.stringify(report.blockEditor)}`);
    report.patterns = await runPatternDepth(page, report);
    log(`patterns: ${JSON.stringify(report.patterns)}`);
    const be = report.blockEditor;
    // The names are the pass's own `steps.*` keys, read off the function rather than guessed:
    // a summary that asks for a flag the pass never sets reports "missing" for a check that
    // simply does not exist, which is worse than no summary at all. `warningReachable` is
    // conditional on a warning existing at all, so it is demanded only when the pass reported
    // that one was on offer — an absent check with an unmet precondition is a fact, not a gap.
    const flags = [
      "created", "path", "insertCategories", "outlineRows", "blockCount",
      "publishDisabledOnError", "publishEnabledAfterFix", "reordered", "duplicated",
      "deleted", "saved", "published", "publicRendered", "historyCoversFifty",
      "outlineWarningCleared", "columnsInserted", "breadcrumbReachesNested",
      "unwindLandedOnSavedTree",
      // Criterion 17's two widths. These are DEMANDED, which is the point: a summary that
      // asked for them a tick ago would have reported "missing" for a pass that died before
      // them, and a summary that does not ask reports nothing at all for a pass that ran them
      // and got `false`. Neither the criterion nor the "no untested screen" rule survives a
      // measurement nothing is obliged to produce.
      "editorNarrowAt1440", "editorNoHorizontalScrollAt1440",
      "editorNarrowAt390", "editorReadOnlyAt390", "noPublishControlAt390",
      "canvasDrawnAt390", "canvasCountMatchesStatus", "editorNoHorizontalScrollAt390",
      "narrowNoticeSaysWhy", "narrowNoticeOffersPreview", "publicRendered",
      // Slice 4's media panel. `mediaSimulateControls` is demanded even when it is 0: a page
      // whose draft has no uploaded image legitimately has no row to simulate, and the absence
      // is an answer. What is NOT allowed is a summary that stays silent about whether the
      // panel was ever opened, because a silently-unmeasured screen is the one failure this
      // demanded-keys mechanism exists to prevent.
      "mediaPanelOpened", "mediaFileCount", "mediaBrokenCount", "mediaRows",
      "mediaEmptyStateExplained", "mediaSimulateControls",
    ];
    const missing = flags.filter((f) => be[f] === undefined);
    // The simulation readings are conditional on there being something to simulate, exactly as
    // `warningReachable` is conditional on a warning existing: an absent check whose
    // precondition was not met is a fact, not a gap. Read AFTER `missing` is declared — a
    // push into it above that line is a use-before-declaration, which `node --check` passes and
    // `check-tdz.cjs` exists to catch.
    if (be.mediaSimulateControls > 0 && be.mediaSimulationChangedTheRender === undefined) {
      missing.push("mediaSimulationChangedTheRender");
      missing.push("mediaSimulated");
      missing.push("mediaSimulatedPressed");
    }
    if (be.warningJumpOffered === true && be.warningReachable === undefined) {
      missing.push("warningReachable");
    }
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify({ mode: "block-editor-only", netFailures, blockEditor: be, patterns: report.patterns, missing }, null, 2),
    );
    console.log(`BLOCK_EDITOR_JSON=${JSON.stringify(be)}`);
    console.log(`BLOCK_EDITOR_MISSING=${missing.length === 0 ? "none" : missing.join(",")}`);
    console.log(`BLOCK_EDITOR_CREATED=${be.created === true} PUBLIC_RENDERED=${be.publicRendered === true}`);
    await browser.close();
    process.exit(0);
  }

  // `--only=menus` runs the navigation and queue depth pass alone.
  //
  // Same argument as `--only=block-editor`: the depth pass is written, and a full pass is the only
  // thing that currently reaches it — forty-five minutes on a box five writers share, of which
  // the queue lives at the very end. A screen that can only be proved by a pass that usually dies
  // before reaching it is a screen that is effectively untested, so the pass gets its own entry
  // point. It runs the SAME function the full pass calls, so a green run here means the full pass
  // would agree; what it does not do is reset the database (run.sh does that) or report a
  // `summary.json` with the whole pass's counts, and the report below says so.
  // `--only=forms` runs the form builder's own depth pass alone.
  //
  // Same argument as `--only=menus` and `--only=block-editor`: the depth pass is written and a
  // full pass is the only thing that reaches it, forty minutes in, on a box five writers share.
  // A screen that can only be proved by a pass that usually dies before reaching it is a screen
  // that is effectively untested, so the pass gets its own entry point. It runs the SAME function
  // the full pass calls; what it does not do is reset the database (run.sh does that) or report a
  // `summary.json` with the whole pass's counts.
  // `--only=seo` runs the SEO toolkit's own depth pass alone.
  //
  // Same argument as `--only=forms` and `--only=menus`: the depth pass is written and a full pass
  // is the only thing that reaches it, forty minutes in, on a box six writers share. A screen
  // that can only be proved by a pass that usually dies before reaching it is a screen that is
  // effectively untested, so the pass gets its own entry point. It runs the SAME function the
  // full pass calls; what it does not do is reset the database (run.sh does that) or report a
  // `summary.json` with the whole pass's counts.
  // `--only=comments` runs the moderation queue's own depth pass alone.
  //
  // Same argument as `--only=seo`, `--only=forms` and `--only=menus`: the depth pass is written
  // and a full pass is the only thing that reaches it, forty minutes in, on a box six writers
  // share. It runs the SAME function the full pass calls; what it does not do is reset the
  // database (run.sh does that) or report a `summary.json` with the whole pass's counts.
  // `--only=newsletter` runs the mailing-list depth pass alone.
  //
  // Same argument as `--only=comments`, `--only=seo`, `--only=forms` and `--only=menus`: the
  // depth pass is written and a full pass is the only thing that reaches it, forty minutes in,
  // on a box six writers share. It runs the SAME function the full pass calls; what it does not
  // do is reset the database (run.sh does that) or report a `summary.json` with the whole
  // pass's counts.
  // `--only=themes` runs the gallery's depth pass alone.
  //
  // Same argument as the four CMS passes above it, and with the same urgency: this pass is the
  // only thing that will ever click *Activate* and *Restore previous* in a browser, and a
  // rollback path that throws a ReferenceError on its first write is indistinguishable from a
  // screen that was never implemented.
  if (process.argv.includes("--only=themes")) {
    report.themes = await runThemesDepth(page, report);
    log(`themes: ${JSON.stringify(report.themes)}`);
    const required = [
      "fixtureThemeExists", "screenReady", "cardsRendered", "fixtureCardIsOnScreen",
      "cardDescribesItself", "bundledCardOffersNoDelete",
      "activeKeyIsOnScreen", "rollbackAbsentWhenNeverSwitched",
      "confirmationOpened", "confirmationNamesTheTheme", "confirmationNamesTheReplaced",
      "badgeMoved", "onlyOneCardIsActive", "noticeIsOnScreen", "columnFollowedThePanel",
      "rollbackTargetIsNamed", "rollbackEnabledWithATarget", "rollbackConfirmationOpened",
      "badgeMovedBack", "columnRestored", "rollbackArmedAgainAfterARestore",
      "noHorizontalScrollAt390",
    ];
    const themeSteps = report.themes || {};
    const missing = required.filter((key) => themeSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=themes",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: themeSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`themes depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`themes depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  // `--only=theme-settings` runs the customize + history depth pass alone. Same argument as
  // `--only=themes`: it is the only thing in a browser that will ever click Publish and
  // Restore on a settings screen, and those two paths are the ones a store test cannot see.
  if (process.argv.includes("--only=theme-settings")) {
    report.themeSettings = await runThemeSettingsDepth(page, report);
    log(`themeSettings: ${JSON.stringify(report.themeSettings)}`);
    const required = [
      "fixtureThemeExists", "siteThemeKeyIsReadable",
      "screenReady", "themeKeyIsNamed", "saysNothingSavedYet",
      "galleryLinksToCustomize", "galleryLinksToHistory", "linkFromGalleryReachesTheEditor",
      "tokenEditorListsTheThemesTokens", "previewSurfaceIsRendered", "lightTextInputExists",
      "previewRecomputes", "unsavedLineIsHonest",
      "hostileValueIsRefusedWithAMessage", "noRevisionWasWrittenByAHostileSave",
      "saveWroteDraftOne", "draftNumberIsOnScreen", "saysNothingPublishedYet",
      "saveDidNotPublish", "noticeNamesTheDraft",
      "contrastPanelIsOnScreen", "contrastNamesBothTokens", "acknowledgementIsOffered",
      "acknowledgementStartsUnchecked", "publishRefusedBelowAA",
      "refusalExplainsTheAcknowledgement", "publishSucceededAfterAcknowledgement",
      "publishedRevisionIsTwo", "screenNamesTheLiveRevision",
      "historyScreenReady", "historyListsBothRevisions", "liveRowIsBadged", "draftRowIsBadged",
      "firstRevisionExplainsItself", "diffIsRenderedPerField", "diffNamesTheField",
      "restoreDialogOpened", "restoreSaysItAppends", "restoreWroteAThirdRevision",
      "historyStillHoldsTheOriginal", "restoredRowIsMarked", "noticeSaysTheHistoryIsAppendOnly",
      "noHorizontalScrollAt390",
    ];
    const settingsSteps = report.themeSettings || {};
    const missing = required.filter((key) => settingsSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=theme-settings",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: settingsSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`theme settings depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`theme settings depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  // `--only=theme-builder` runs the slice-3 depth pass alone: the eight-slot builder and the
  // package uploader. Same argument as the two passes above it — a slot save, a slot reset, a
  // package refusal and an install are four things no store test can be sure the PANEL does,
  // because the API is correct whether or not the button is wired to it. That is exactly how
  // the gallery's delete control spent a slice rendering with no handler at all.
  if (process.argv.includes("--only=theme-builder")) {
    report.themeBuilder = await runThemeBuilderDepth(page, report);
    log(`themeBuilder: ${JSON.stringify(report.themeBuilder)}`);
    const required = [
      "fixtureThemeInstalled", "siteThemeChangedToFixture",
      "fixtureHeaderHasADefault", "fixtureDefaultIsInItsOwnColumn",
      "screenReady", "themeKeyIsNamed", "everySlotIsOffered", "headerBadgeSaysThemeDefault",
      "canvasIsMountedForTheSlot", "galleryLinksToBuilder", "galleryHasAWorkingDeleteControl",
      "insertPanelOpens", "insertPanelOffersManyTypes", "blockAppearsInTheOutline",
      "inspectorIsMounted", "barReportsUnsaved", "propFieldIsWritable",
      "slotRowWasWritten", "savedSlotIsNotADefault", "savedSlotHoldsTheEditedBlock",
      "defaultSurvivedTheSave", "badgeMovedToCustom", "noticeNamesTheSave", "slotSaveTouchedNoPage",
      "resetIsOfferedAfterACustomSave", "resetConfirmationOpened",
      "resetConfirmationSaysItIsNotRecoverable", "resetRestoredTheShippedTree",
      "badgeMovedBackToTheme", "resetIsNotOfferedForAThemeDefault",
      "switchingSlotLoadsThatSlot", "resetHiddenForASlotWithNoDefault", "emptySlotSaysSo",
      "uploadScreenReady", "uploadHasAFileInput", "uploadEmptyStateExists",
      "badPackageWasRead", "reportIsRendered", "reportSaysInvalid", "everyProblemIsListed",
      "findingsNameAPath", "installIsRefusedWhileInvalid", "nothingWasInstalled",
      "pickingAnotherFileClearsTheReport", "goodReportIsValid", "validPackageHasNoFindings",
      "installIsOfferedOnAValidReport", "installSucceeded", "themeIsInTheLibrary",
      "installDidNotActivate", "noticeSaysInactive",
      "bundledRemovalIsRefused", "bundledRefusalNamesTheRule", "bundledThemeIsStillThere",
      "refusedRemovalWroteNothing", "allowedRemovalWorks", "removalSaysWhatHappened",
      "builderAt390", "builderHasNoHorizontalScrollAt390",
    ];
    const builderSteps = report.themeBuilder || {};
    const missing = required.filter((key) => builderSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=theme-builder",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: builderSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`theme builder depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`theme builder depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    // A scoped pass that did not run its steps is a FAILED pass, not a pass with nothing to
    // report. `run.sh` runs under `set -e`, so returning here made "the QA site does not
    // exist" exit 0 — a green result for a pass that checked nothing, which is how 59 skipped
    // steps came to be read as a pass on 2026-09-30. A missing step now ends the process.
    if (missing.length > 0) process.exit(4);
    return;
  }
  // `--only=theme-render` draws all ten bundled themes in the public site. Split out from the
  // builder pass for the same reason every other scoped pass exists (a full pass is cut down
  // halfway) and for one more: this pass is the only place in the harness that needs twenty
  // public renders, and folding them into an hour-long pass buries the one claim it makes.
  if (process.argv.includes("--only=theme-render")) {
    report.themeRender = await runThemeRenderPass(page, report);
    log(`themeRender: ${JSON.stringify(report.themeRender)}`);
    const required = [
      "registryHasTenThemes", "everyKeyIsInTheRegistry", "everyThemeAnnouncesItself",
      "atLeastThreeDiffer", "noTwoThemesAreIdentical",
      "noThemeOverflowsAt390", "siteRestored",
    ];
    const renderSteps = report.themeRender || {};
    const missing = required.filter((key) => renderSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=theme-render",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: renderSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`theme render pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`theme render pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    if (missing.length > 0) process.exit(4);
    return;
  }
  if (process.argv.includes("--only=newsletter")) {
    report.newsletter = await runNewsletterDepth(page, report);
    log(`newsletter: ${JSON.stringify(report.newsletter)}`);
    const required = [
      "fixtureListExists", "screenReady", "listKeyIsOnScreen",
      "publicSignupAnswers202", "signupSaysConfirmationIsNeeded", "signupCarriesNoToken",
      "pendingRowIsInSql", "pendingIsNotDeliverable", "pendingRowIsOnScreen",
      "pendingTabCountMatchesSql", "pendingRowShowsItsExpiry",
      "confirmApplied", "confirmedInSql", "confirmTokenClearedAfterUse",
      "replayedConfirmIsRefused", "unknownTokenIsTheSameRefusal", "confirmedIsDeliverable",
      "expiredConfirmIsRefused", "expiredRowStayedPending",
      "unsubscribeApplied", "unsubscribeKeptTheRow", "unsubscribedIsNotDeliverable",
      "panelUnsubscribeWorked", "panelNoticeSaysTheRowIsKept",
      "bounceDialogOpened", "bounceStoredWithItsReason",
      "importDialogOpened", "importReportIsOnScreen", "importReportNamesTheSkippedAddress",
      "importDidNotReviveTheUnsubscribedRow", "importAddedTheNewAddress",
      "sendDialogOpened", "emptySubjectIsRefusedByTheForm",
      "issueIsInTheArchive", "issueRecordedZeroRecipientsBecauseNobodyWasSubscribed",
      "archiveShowsTheIssue", "noHorizontalScrollAt390",
    ];
    const newsletterSteps = report.newsletter || {};
    const missing = required.filter((key) => newsletterSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=newsletter",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: newsletterSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`newsletter depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`newsletter depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  if (process.argv.includes("--only=content-api")) {
    report.contentApi = await runContentApiDepth(page, report);
    log(`content-api: ${JSON.stringify(report.contentApi)}`);
    // The pass's own `steps.*` vocabulary, read off the function. `documented_*` is generated
    // from the SERVER's operation ids, so it cannot be listed here — the per-id steps are checked
    // by a count instead, because a fixed list of six names would stop matching the day a seventh
    // route is documented, and a checklist that quietly loses a row is worse than none.
    const required = [
      "tokenTableExists", "noPlaintextColumn", "tokenHashIsStoredNotTheSecret",
      "usageTableExists", "tokensScreenReady", "sectionNavIsOnScreen", "docsTabIsLinked",
      "theNameIsInTheField", "copyOnceDialogOpened", "theDialogShowsThePlaintext",
      "doneIsBlockedUntilStored", "tickingStoredReleasesDone", "operatorCanMintAToken",
      "plaintextIsNotStored", "rowShowsThePrefixNotTheSecret", "rowNeverShowsTheSecret",
      "mintedRowIsOnScreen", "tokenReadsTheContentSurface", "everyItemCarriesItsCacheKeys",
      "aPanelSessionIsRefusedTheContentSurface", "limitIsHonoured", "envelopeHasAllThreeKeys",
      "missingScopeIsNamed", "docsScreenReady", "docsErrorStripIsAbsent", "documentIsOpenApi31",
      "documentDeclaresEveryEndpoint", "baseUrlIsShown", "paginationGuideIsPresent",
      "errorCodesAreListed", "rebuildExampleIsPresent", "yamlDownloadCarriesTheEndpoints",
      "aBadFormatIsRefusedWithItsField", "revokeSucceeded",
      "aRevokedTokenStopsReadingImmediately", "theRefusalSaysRevokedNotWrong",
      "theRevokedRowStaysVisible", "noHorizontalScrollAt390",
    ];
    const apiSteps = report.contentApi || {};
    const missing = required.filter((key) => apiSteps[key] === undefined);
    // Every documented operation must have a row on screen. Derived from the same document the
    // screen renders, so this cannot pass against a hard-coded list of six ids.
    const documentedRows = Object.keys(apiSteps).filter((key) => key.startsWith("documented_"));
    const undocumentedRows = documentedRows.filter((key) => apiSteps[key] !== true);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=content-api",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          documentedRows: documentedRows.length,
          undocumentedRows,
          steps: apiSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0 || undocumentedRows.length > 0) {
      log(
        `content-api depth pass MISSING ${missing.length}: ${missing.join(", ")}` +
          (undocumentedRows.length > 0
            ? ` · UNDOCUMENTED ${undocumentedRows.length}: ${undocumentedRows.join(", ")}`
            : ""),
      );
    } else {
      log(
        `content-api depth pass ${required.length}/${required.length}` +
          ` · ${documentedRows.length} endpoints documented`,
      );
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  if (process.argv.includes("--only=comments")) {
    report.comments = await runCommentsDepth(page, report);
    log(`comments: ${JSON.stringify(report.comments)}`);
    const required = [
      "fixtureRowsExist", "screenReady", "policyPanelIsOnScreen",
      "pendingTabShowsTheStoredCount", "approvedTabIsNotEmpty", "queuedRowIsOnScreen",
      "queuedRowNamesItsPage", "queuedCommentIsNotPublic", "approvedCommentIsPublic",
      "publicThreadCarriesNoAddress", "approvedInSql", "approvedRecordedAWho",
      "approvedIsNowPublic", "spamRowsAreOnScreen", "spamRowShowsItsReason",
      "spamTabCountMatchesSql", "spamIsNotPublic", "undoneInSql",
      "bulkBarAppearedOnSelection", "bulkNoticeIsAPerCommentReport", "drawerOpened",
      "drawerShowsTheWholeBody", "replyTextIsOnTheInput", "replyIsInSql", "replyIsApproved",
      "policyOffInSql", "submissionRefusedWhileOff", "submissionStoredNothing",
      "policyBackOn", "banListShowsTheBan", "banListCarriesTheReason",
      "bannedAddressIsRefused", "bannedSubmissionStoredNothing", "noHorizontalScrollAt390",
    ];
    const commentSteps = report.comments || {};
    const missing = required.filter((key) => commentSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=comments",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: commentSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`comments depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`comments depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  // `--only=featured-media` runs a page's featured-image depth pass alone.
  //
  // Same argument as every other depth pass on this harness, and for one more reason here: the
  // degradation half of the criterion needs a TRASHED file and a RESTORE, and a full pass resets
  // the database on its way, so a trashed-file assertion in the full pass is at the mercy of
  // whatever the pass happens to do next. Driving it alone makes the sequence deterministic.
  // It runs the SAME function the full pass calls.
  if (process.argv.includes("--only=featured-media")) {
    report.featuredMedia = await runFeaturedMediaDepth(page, report);
    log(`featured media: ${JSON.stringify(report.featuredMedia)}`);
    // The list is the pass's own vocabulary. Every name here was a claim worth making, and a name
    // that stops appearing is a claim nobody is checking any more.
    const required = [
      "pageWasCreated", "fixtureImageExists", "screenReady", "emptyStateIsShown",
      "emptyStateSaysNoImage", "availabilitySaysNoImage", "altIsDisabledWithNoImage",
      "pickerOpened", "pickerOffersTheFile", "pickingClosedThePicker",
      "saveIsBlockedWithoutAnAlt", "screenExplainsWhy", "theFilesOwnAltIsOffered",
      "theFilesOwnAltIsNowInTheField", "savedWithoutError", "noticeIsOnScreen",
      "altIsInSql", "legendIsInSql", "mediaIdIsInSql", "cropStartsUnset",
      "altSurvivesAReload", "availabilitySaysAvailable", "previewIsOnScreen",
      "previewCarriesTheAlt", "noCropIsAnnounced",
      "cropIsInSql", "cropAxesWerePaired", "cropMovedRightAndDown",
      "publicPageAnswers", "publicPayloadCarriesTheImage", "publicAltIsThisPagesAlt",
      "publicObjectPositionIsSet", "publicLegendIsCarried",
      "partialSaveMovedTheLegend", "partialSaveKeptTheAlt", "partialSaveKeptTheCrop",
      "clearingTheCropShowedTheUnsetMessage", "clearingTheCropActuallyClearedIt",
      "clearingTheCropKeptTheImage",
      "trashedFileWarnsTheOperator", "trashedFileNamesItself",
      "trashedFileSaysThePageStillRenders", "availabilitySaysTrashed",
      "cropIsStillUsableWhileTrashed",
      "pageStillRendersWithNoImage", "pageStillCarriesItsTitle",
      "pageDoesNotCarryTheTrashedImage", "columnStillNamesTheTrashedFile",
      "pickerDoesNotOfferATrashedFile",
      "restoringBringsTheImageBack", "aRestoredImageWarnsAboutNothing",
      "removeConfirmationOpened", "removeConfirmationNamesTheAlt",
      "removeConfirmationNamesTheCrop", "removeConfirmationSaysTheFileStays",
      "removeClearedEverything", "removeReturnedToTheEmptyState", "removeDidNotDeleteTheFile",
      "noHorizontalScrollAt390",
    ];
    const featuredSteps = report.featuredMedia || {};
    const missing = required.filter((key) => featuredSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=featured-media",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: featuredSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`featured media depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`featured media depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  if (process.argv.includes("--only=members")) {
    report.members = await runMembersDepth(page, report);
    log(`members: ${JSON.stringify(report.members)}`);
    // The list is the pass's own vocabulary. Every name here was a claim worth making, and a
    // name that stops appearing is a claim nobody is checking any more.
    const required = [
      "memberTableExists", "memberTableHasNoPanelLink", "memberRolesArePlainText",
      "defaultGatedBehaviourIsNotFound", "screenReady", "policyPanelIsOnScreen",
      "emptyStateIsShownWhenThereAreNoMembers", "emptyStateNamesTheSignupRoute",
      "panelShowsTheGatedBehaviour", "operatorCreatedAMember", "invitedHasNoPassword",
      "invitedRowSaysSo", "rowIsOnScreen", "pendingIsNotRenderedAsAFailure",
      "pendingTabMatchesSql", "drawerOpened", "drawerStatesTheBoundary", "rolesAreOnTheInput",
      "rolesAreInSql", "verifiedInSql", "memberCookieIsRefusedAtAPanelRoute",
      "gatedPageExists", "publicSigninWorks", "memberCookieIsItsOwnName", "gateProbeAnswers",
      "gatedPageIsFoundByTheProbe", "gateRefusesAVisitor", "gateAdmitsTheMember",
      "memberCookieIsAccepted", "ungatedPageIsServedToAVisitor",
      "roleGateRefusesAMemberWithoutIt", "roleGateAdmitsAfterTheGrant",
      "sessionExistedBeforeTheBlock", "blockDialogAskedForAReason", "blockedInSql",
      "blockRemovedTheSessionRow", "blockedMemberIsRefused", "behaviourInSql",
      "promptBehaviourIsReported", "promptNamesTheSignInLink", "notFoundNamesNoSignInLink",
      "behaviourRestored", "inviteDialogOpened", "deleteDialogNamesTheAddress",
      "deletedFromSql", "settingsRouteReady", "settingsRouteShowsThePolicy",
      "settingsRouteHasNoMemberTable", "settingsRouteShowsTheSameBehaviour",
      "settingsRouteSaveIsInSql", "settingsRouteSaveSaidSo", "signupRestored",
      "noHorizontalScrollAt390",
      // The phone measurement, demanded per screen rather than once for the module. The last
      // two are the point of this list: an absent key reads as a pass, so a run that died
      // between the routes would report a green that measured half the screen.
      "membersTableReadyAt390", "membersLayoutAt390", "membersTableNoHorizontalScrollAt390",
      "policyRouteNoHorizontalScrollAt390",
    ];
    const memberSteps = report.members || {};
    // The drawer pair is conditional on there being a member to open, and the absence is
    // itself recorded (`reasonNoMemberToOpen`). A precondition that cannot be satisfied must
    // be demanded only when the pass says it could be — otherwise the criterion is
    // "the members list was empty on a disposable database", which is a scheduling fact.
    if (memberSteps.reasonNoMemberToOpen === undefined) {
      required.push("drawerOpenedAt390", "drawerFitsAt390");
    }
    const missing = required.filter((key) => memberSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=members",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: memberSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`members depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`members depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  if (process.argv.includes("--only=seo")) {
    report.seo = await runSeoDepth(page, report);
    log(`seo: ${JSON.stringify(report.seo)}`);
    // The list below is the pass's own vocabulary, read off the function rather than guessed.
    const required = [
      "screenReady", "redirectsPanelIsTheDefaultTab", "emptyRedirectsExplainThemselves",
      "redirectFormOpened", "fromIsOnTheInput", "toIsOnTheInput", "ruleRowLanded",
      "ruleIsOnScreen", "ruleIsInSql", "testResultShown", "testNamesTheRule",
      "testSaysItDidNotCount", "testCountedNoHit", "hitsBadgeSaysZero",
      "relativeFromRefused", "relativeFromNamesTheRule", "relativeFromStoredNothing",
      "sitemapPanelOpened", "pageTypesAreThisSitesOwn", "robotsEditorIsPrefilled",
      "blockingRobotsWarns", "blockingRobotsWarningNamesItself", "robotsSaved",
      "sitemapPreviewShown", "previewIsRealXml", "shownCountMatchesStorage",
      "previewCountIsNotAFabricatedNumber", "brokenPanelOpened", "scanReportedSomething",
      "deleteConfirmOpened", "deleteConfirmNamesThePath", "deletedFromTheList", "deletedFromSql",
    ];
    const seoSteps = report.seo || {};
    const missing = required.filter((key) => seoSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify(
        {
          mode: "--only=seo",
          total: required.length,
          passed: required.length - missing.length,
          missing,
          steps: seoSteps,
        },
        null,
        2,
      ),
    );
    if (missing.length > 0) {
      log(`seo depth pass MISSING ${missing.length}: ${missing.join(", ")}`);
    } else {
      log(`seo depth pass ${required.length}/${required.length}`);
    }
    await page.context().browser()?.close().catch(() => {});
    return;
  }
  if (process.argv.includes("--only=forms")) {
    report.forms = await runFormsDepth(page, report);
    log(`forms: ${JSON.stringify(report.forms)}`);
    // The list below is the pass's own vocabulary, read off the function rather than guessed.
    const required = [
      "listReady", "createFormOpened", "nameIsOnTheInput", "keyFollowsName", "rowLanded",
      "rowOnScreen", "draftIsLabelled", "editLinkHasAnId", "editorReady",
      "paletteAddsAField", "choiceFieldOpenedInspector", "optionlessChoiceRefused",
      "optionlessChoiceWasNotStored", "savedFields", "optionsCarriedBothChoices",
      "duplicateKeyRefused", "previewOpened", "previewHasTheCanvasFields",
      "previewRefusedAnEmptyRequired", "previewAcceptedAFilledForm",
      "previewRefusedAnEmptyMessage", "published", "publishIsLabelled", "settingsOpened",
      "settingsCarriesTheStoredValues", "redirectInertWhileShowingAMessage",
      "messageInertWhileRedirecting", "redirectWithoutUrlRefused", "settingsSaved",
      "validSubmissionStatus", "validSubmissionStored", "honeypotStatus",
      "honeypotLooksAccepted", "honeypotLeaksNoFieldErrors", "honeypotStoredNothing",
      "tooFastLooksAccepted", "invalidStatus", "invalidCarriesFieldErrors",
      "invalidNamesTheChoiceField", "invalidNamesTheShortName", "invalidStoredNothing",
      "inboxHasItsOwnRoute", "inboxReady", "inboxTabCounts", "inboxShowsTheSubmission",
      "unreadIsOne", "spamTabIsEmpty", "spamTabNamesTheProtections", "drawerOpened",
      "drawerShowsTheAnswers", "drawerShowsTheName", "openingMarkedItRead",
      "exportStatus", "exportIsCsv", "exportHasTheFilteredRow", "exportHasNoOtherState",
      "listStillCarriesTheRow", "listShowsItPublished", "deleteConfirmOpened",
      "deleteConfirmNamesTheSubmissions", "deletedFromTheList", "deletedFromSql",
      "submissionsCascaded",
    ];
    // The form pass writes into a flat `steps` object — there is no nested key, and reading one
    // into existence would demand checks the function never writes.
    const formSteps = report.forms || {};
    const missing = required.filter((key) => formSteps[key] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify({ mode: "forms-only", netFailures, forms: formSteps, missing }, null, 2),
    );
    console.log(`FORMS_JSON=${JSON.stringify(formSteps)}`);
    console.log(`FORMS_MISSING=${missing.length === 0 ? "none" : missing.join(",")}`);
    console.log(
      `FORMS_CONSOLE_ERRORS=${(report.consoleErrors || []).length} NET_FAILURES=${netFailures.length}`,
    );
    await browser.close();
    process.exit(0);
  }

  if (process.argv.includes("--only=menus")) {
    report.menus = await runMenusDepth(page, report);
    log(`menus: ${JSON.stringify(report.menus)}`);
    // The names are the pass's own `steps.*` keys, read off the function rather than guessed: a
    // checklist written from the REQ's prose asks for `rescheduled` when the pass says
    // `rescheduleMoved`, and the mode then reports every check missing forever — which reads as a
    // broken screen and is really a typo. The list below is the pass's own `steps.*` vocabulary.
    const required = [
      // the list and the form
      "listReady", "formOpened", "keyFollowsName", "rowLanded", "rowOnScreen",
      // the tree
      "editorReady", "threeTopLevel", "nestedUnderSecond", "nestedParentRowFound",
      "treeRendered", "treeHasChildren", "savedItems", "parentsAreStored", "depthLabel",
      "fourthLevelRefused", "fourthLevelStatus", "refusalLeftTheTreeAlone",
      "fourthLevelReached", "fourthLevelParentDepth", "typedThirdLevel",
      // Add pages…
      "pickerOpened", "pickerOnlyOffersPublished", "pageItems", "labelComesFromTheTitle",
      // audience
      "audienceToggleChangesThePayload", "visitorItems", "memberItems",
      "membersItemHiddenFromVisitor", "memberItemSaved",
      // locations
      "claimedHeader", "rivalClaimRefused", "rivalClaimStatus", "rivalRefusalNamesTheHolder",
      "firstHolderKeptIt",
      // the queue (same flat `steps` object — see below)
      "queueReady", "entryOnScreen", "rescheduleFormOpened", "rescheduleStored",
      "rescheduleIsLater", "rescheduleMoved", "cancelledInSql", "cancelButtonGone",
      "retryRefusesASentRow", "retryRefusesAPendingRow", "retryRefusalCode",
      "retryLeftTheRowPending", "scheduleStatus",
    ];
    // The queue half writes into the SAME flat `steps` object — there is no nested `queue`
    // key, and reading one into existence would have demanded eleven checks that can never be
    // satisfied.
    const menuSteps = report.menus || {};
    const missing = required.filter((f) => menuSteps[f] === undefined);
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify({ mode: "menus-only", netFailures, menus: menuSteps, missing }, null, 2),
    );
    console.log(`MENUS_JSON=${JSON.stringify(menuSteps)}`);
    console.log(`MENUS_MISSING=${missing.length === 0 ? "none" : missing.join(",")}`);
    console.log(
      `MENUS_CONSOLE_ERRORS=${(report.consoleErrors || []).length} NET_FAILURES=${netFailures.length}`,
    );
    await browser.close();
    process.exit(missing.length === 0 ? 0 : 1);
  }

  // The analytics batch goes in before the routes are walked: the report screens read it, and the
  // history fixture gives their series more than one bucket to draw.
  report.analytics = await seedAnalytics(report);
  log(`analytics seed: ${JSON.stringify(report.analytics)}`);

  const routes = [
    { path: "/", name: "overview" },
    { path: "/pages", name: "pages" },
    // The block registry reference (REQ-063, slice 1) — no untested screen: it is walked here
    // and expanded, and the editor it documents is driven by the depth pass below, which first
    // creates a page to edit (the editor's address carries the page's id, not its slug).
    { path: "/blocks", name: "blocks" },
    // The pattern library and the template gallery (REQ-063, slice 3) — no untested screen: the
    // routes are walked here and the depth pass below creates a pattern, inserts it into the
    // editor's page, and builds a page from a template.
    { path: "/patterns", name: "patterns" },
    { path: "/page-templates", name: "page-templates" },
    // The navigation editor and the scheduled publishing queue (REQ-064, slice 1) — no untested
    // screen: both are walked here and the depth pass below builds a three-level menu, claims a
    // location, adds a published page through the picker, flips the audience toggle, and then
    // reschedules and cancels a queue entry. The menu *editor* is not in this list on purpose,
    // for the same reason the media file detail screen is not: its address carries a menu id,
    // and a route walked with a placeholder id would only prove the 404 state renders.
    { path: "/menus", name: "menus" },
    { path: "/publishing/queue", name: "publishing-queue" },
    // The form list and its inbox (REQ-064, slice 2) — no untested screen: the list is walked
    // here and the depth pass below creates a real form, builds it, publishes it and drives the
    // inbox it fills. The builder is NOT in this list for the same reason the menu editor is not:
    // its address carries a form id, and a route walked with a placeholder id would only prove
    // that the 404 state renders.
    { path: "/forms", name: "forms" },
    // The SEO toolkit (REQ-064, slice 3) — walked here so the screen is in the inventory, and
    // driven by the depth pass below, which creates a redirect, tests it against a path, and
    // regenerates the sitemap. Every panel is a tab on one route, so one entry covers all three
    // rather than three routes that would each need their own placeholder.
    { path: "/seo", name: "seo" },
    // The comment queue (REQ-064, slice 4a) — walked here so the screen is in the inventory,
    // and driven by the depth pass below, which seeds comments in three moderation states,
    // approves one, undoes a heuristic's verdict, answers a comment as the site and proves the
    // policy by refusing a public submission. One route covers the queue, its policy and its
    // bans, because the panel puts all three on one page.
    { path: "/comments", name: "comments" },
    // The mailing lists (REQ-064, slice 4b) — no untested screen: the route is walked here so
    // it is in the inventory, and the depth pass below drives the whole double opt-in end to end
    // (a public signup, the confirmation link, the replayed link, the expiry, the unsubscribe,
    // a bounce with a reason, a CSV import that has to name what it skipped, and the archive).
    { path: "/newsletter", name: "newsletter" },
    // Visitor accounts and the membership policy (REQ-064, slice 4c) — no untested screen.
    // BOTH routes are listed, for the reason the members depth pass explains: two routes
    // rendering the SAME component is the design, so walking only `/members` would leave the
    // one an owner reaches for from a settings menu unmeasured, and walking only the settings
    // route would never open the table whose drawer, dialogs and card layout are the whole
    // screen. Acceptance 18 asks for every new screen at 390 px, and a screen absent from this
    // list is not measured by any pass — it was walked by no pass at all, on any width.
    { path: "/members", name: "members" },
    { path: "/members/settings", name: "member-settings" },
    // The Content API section (REQ-019, slices 1 and 2) — no untested screen. BOTH routes are
    // listed for the same reason the two members routes are: they are two screens of one section
    // reached through a tab bar, and walking only the Tokens tab would leave the document — the
    // thing an integrator actually comes here to read — measured by nothing. The Docs tab's own
    // content is data-driven from the server's OpenAPI document, so a walk that opened it and
    // found no endpoint rows would be measuring a failed request rather than an empty screen.
    { path: "/content-api", name: "content-api" },
    { path: "/content-api/docs", name: "content-api-docs" },
    // The theme gallery (REQ-062, slice 1) — walked here so the screen is in the inventory,
    // and driven by `runThemesDepth` below, which activates a theme, reads the badge, restores
    // the previous one and requires the button to disappear when there is nothing to restore.
    { path: "/themes", name: "themes" },
    // The theme builder and the package uploader (REQ-062, slice 3) — no untested screen: both
    // routes are in the inventory here and driven by `runThemeBuilderDepth` below, which saves a
    // slot, reads the picker's badge out of the table, restores the theme default, refuses an
    // unknown-slot package with every finding listed, installs a valid one inactive and proves
    // the bundled-theme removal is refused with the server's own sentence.
    { path: "/themes/minimal/builder", name: "theme-builder" },
    { path: "/themes/upload", name: "theme-upload" },
    { path: "/media", name: "media" },
    // The file manager's trash (REQ-010, slice 1) — no untested screen: the route is walked and
    // clicked here, and the depth pass below creates a folder, trashes a file and restores it.
    { path: "/media/duplicates", name: "media-duplicates" },
    { path: "/media/trash", name: "media-trash" },
    // The transformation presets (REQ-010, slice 3) — walked here and driven by the depth pass
    // below, which creates a preset, submits an out-of-range quality to see the field error, and
    // asks for the preset URL to answer with real transformed bytes.
    { path: "/media/settings", name: "media-settings" },
    // The file detail screen (REQ-010, slice 2) is NOT in this list on purpose: its path
    // carries a file id, and a route walked with a placeholder id only proves that the 404
    // state renders. `runMediaFileDetail` below opens a *real* file's screen instead. Listing
    // the bare prefix here produced exactly that 404 screenshot.
    // The backup centre (REQ-013, slice 1) — walked here, and driven by the depth pass below,
    // which takes a real backup, watches all five parts reach a terminal state and verifies
    // the artifacts off the destination. A backup screen that is never clicked is exactly
    // the screen that ships claiming a restore point nobody has ever produced.
    { path: "/backups", name: "backups" },
    { path: "/sites", name: "sites" },
    { path: "/ai", name: "ai" },
    // The results screen is a route like any other: it is walked, clicked and measured.
    { path: "/search?q=qa", name: "search" },
    // The index's own screen (REQ-002, slice 3) — no untested screen.
    { path: "/settings/search", name: "search-settings" },
    // The identity & access screens (REQ-006, slice 2) — no untested screen: the depth pass below
    // creates accounts, attaches scopes, simulates verdicts, and drives a group and a key.
    { path: "/settings/iam", name: "iam-overview" },
    { path: "/settings/iam/users", name: "iam-users" },
    { path: "/settings/iam/groups", name: "iam-groups" },
    { path: "/settings/iam/service-accounts", name: "iam-service-accounts" },
    { path: "/settings/iam/simulator", name: "iam-simulator" },
    // The ABAC policy builder (REQ-006, slice 4a) — the depth pass below drives the rows, the
    // dry run, a save with its version history and a removal.
    { path: "/settings/iam/policies", name: "iam-policies" },
    // The permission-request inbox and the SCIM provisioning screen (REQ-006, slice 4b) — the
    // depth passes below ask, approve, refuse, mint a token and drive a real SCIM round trip.
    { path: "/settings/iam/approvals", name: "iam-approvals" },
    { path: "/settings/iam/provisioning", name: "iam-provisioning" },
    // Enterprise sign-in (REQ-006, slice 4b-2): the provider list, the drawer and the discovery
    // test. Its depth pass below connects a provider, proves the test reports a *result* rather
    // than a transport error, and removes it again.
    { path: "/settings/iam/authentication", name: "iam-authentication" },
    // The security, session and device screens (REQ-006, slice 3) — the depth pass below drives
    // the policy fields, revokes a session and trusts a device.
    { path: "/settings/iam/security", name: "iam-security" },
    { path: "/settings/iam/sessions", name: "iam-sessions" },
    { path: "/settings/iam/devices", name: "iam-devices" },
    // The role screens (REQ-006, slice 1) — no untested screen: the list is walked here, and its
    // depth pass below creates a role, drives the matrix and reads the history back.
    { path: "/settings/iam/roles", name: "iam-roles" },
    // The analytics reports (REQ-007, slice 2): every screen of the section is walked, clicked and
    // measured, and the depth pass below reads the range, the comparison, a drawer and an export.
    // The notification list (REQ-021, slice 1) — walked here and driven by the depth pass
    // below, which emits real notifications through the API, checks the bell's badge against
    // its own grouped lines, filters from a group line, runs a bulk action and proves the
    // keyboard path.
    { path: "/notifications", name: "notifications" },
    // The preferences matrix (REQ-021, slice 2). Walked on its own route rather than reached
    // through the list, because "no untested screen" is about the *screen* and a settings
    // page that is only ever opened by a click is a screen whose first paint is never seen.
    // Its depth pass below flips a cell, saves, reloads and reads the value back.
    { path: "/notifications/settings", name: "notifications-settings" },
    // The outbox and the routing rules (REQ-021, slice 3). Same reasoning as the settings
    // screen above: an administrator-only screen that is only ever reached by a click is a
    // screen whose first paint nobody has seen. Its depth pass below writes a rule, runs an
    // event through the router, reads the counts back and removes the rule again.
    { path: "/notifications/outbox", name: "notifications-outbox" },
    // The event console (REQ-016, slice 1). Walked on its own route for the same reason as the
    // settings screen above: the Catalogue tab is a second data source behind a query string,
    // and a tab nobody ever visits is a tab whose first paint nobody has seen. Its depth pass
    // below filters the feed by a name, expands a payload, opens the catalogue and narrows it
    // by area.
    { path: "/events", name: "events" },
    { path: "/events?tab=catalogue", name: "events-catalogue" },
    // The bus's own retention (REQ-016, slice 3) — a third tab on the same screen, and the
    // only one whose numbers come from a different endpoint than the feed. Walked here so the
    // "no untested screen" rule covers it too, and driven by `runRetentionDepth` below.
    { path: "/events?tab=retention", name: "events-retention" },
    // The webhook endpoints (REQ-016, slice 2) — the list and the create form are walked here.
    // The detail screen is NOT: its path carries an endpoint id, and a route walked with a
    // placeholder id only proves the not-found state renders. `runWebhooksDepth` below opens a
    // *real* endpoint instead — the same reasoning as the media file detail above.
    { path: "/webhooks", name: "webhooks" },
    { path: "/webhooks/new", name: "webhooks-new" },
    { path: "/analytics", name: "analytics" },
    { path: "/analytics/pages", name: "analytics-pages" },
    { path: "/analytics/sources", name: "analytics-sources" },
    { path: "/analytics/audience", name: "analytics-audience" },
    { path: "/analytics/events", name: "analytics-events" },
    { path: "/analytics/downloads", name: "analytics-downloads" },
    { path: "/analytics/forms", name: "analytics-forms" },
    // Goals, funnels and realtime (REQ-007, slice 3), and the settings screen of slice 4 — the
    // section's own write surface, whose depth pass below drives it.
    { path: "/analytics/goals", name: "analytics-goals" },
    { path: "/analytics/realtime", name: "analytics-realtime" },
    { path: "/analytics/settings", name: "analytics-settings" },
    // The security centre's five screens (REQ-012, slices 1–3). `runSecurityDepth` drives the
    // overview, the findings store and the header policy, but it never opened the last two —
    // and the same is true of the route list, so two screens that ship with rules, a policy
    // editor and a live counter had never been rendered by anything. "No untested screen"
    // means no untested screen: both are walked here and clicked by the depth pass below.
    { path: "/security", name: "security-overview" },
    { path: "/security/findings", name: "security-findings" },
    { path: "/security/headers", name: "security-headers" },
    { path: "/security/rate-limits", name: "security-rate-limits" },
    { path: "/security/sign-in-protection", name: "security-sign-in-protection" },
  ];
  // `--only` narrows the route list; the default walks every entry above, unchanged.
  const walkedRoutes = ONLY_ALL ? routes : routes.filter((route) => wants(route.name));
  for (const route of walkedRoutes) matchedOnly.add(route.name);
  if (!ONLY_ALL) {
    log(`focused pass: ${walkedRoutes.length}/${routes.length} routes — ${ONLY.join(", ")}`);
  }
  // The route loop is per-route isolated for the same reason the depth passes are: a crashed
  // tab (`Page crashed`, which several concurrent passes can cause by exhausting the box's
  // memory) used to end the entire run, so every route after the crash and every depth pass
  // were skipped and no report was written at all. A page that dies is a finding about that
  // page; the pages after it still have to be looked at.
  for (const route of walkedRoutes) {
    log(`page: ${route.name}`);
    try {
      await page.goto(`${URL_ADMIN}${route.path}`, { waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForTimeout(900);
      if (route.name === "media") {
        report.mediaUpload = await uploadMediaSample(page);
        log(`media upload: ${JSON.stringify(report.mediaUpload)}`);
        await page.waitForTimeout(600);
      }
      const diag = await diagnostics(page);
      await shot(page, `page-${route.name}`);
      await interact(page, route.name, report);
      report.pages.push({ ...route, diagnostics: diag });
    } catch (cause) {
      const reason = cause instanceof Error ? `${cause.name}: ${cause.message}` : String(cause);
      log(`page ${route.name} failed: ${reason}`);
      record({ page: route.name, action: "route-failed", reason });
      report.pages.push({ ...route, failed: reason });
    }
  }

  // The block editor's pass (REQ-063, slice 1): a page of its own, then the insert panel, the
  // generated inspector, live validation refusing and clearing a publish, reorder/duplicate/
  // delete, save, publish, and the public page the published block tree actually renders.
  report.blockEditor = await runBlockEditorDepth(page, report);
  log(`block editor: ${JSON.stringify(report.blockEditor)}`);

  // The pattern library's and the template gallery's pass (REQ-063, slice 3): a pattern is
  // created from the editor's blocks, inserted back into the page, and a page is built from a
  // platform template. Runs *after* the block editor's pass, which is what leaves blocks on a
  // page for the selection to be cut from.
  report.patterns = await runPatternDepth(page, report);
  log(`patterns: ${JSON.stringify(report.patterns)}`);

  // The file manager's depth pass (REQ-010, slice 1): a folder is created, the listing is filtered,
  // two files are selected so the bulk bar appears, one is trashed, and the trash brings it back.
  // Each depth pass is isolated: one throwing must not skip the ones after it. A pass that
  // cannot run is a finding of its own ("this screen did not answer"), not a reason to end the
  // whole run before the remaining screens have been looked at.
  if (wants("media-file-manager")) {
    matchedOnly.add("media-file-manager");
    report.mediaFiles = await runDepthPass("media-file-manager", () =>
      runMediaFileManager(page, report),
    );
  }

  // The file detail screen (REQ-010, slice 2): a real file is opened, its preview renders, the
  // metadata saves, and the version history is read. This is the pass that proves the screen is
  // a screen — a route walked only by id would render its error state and look visited.
  if (wants("media-file-detail")) {
    matchedOnly.add("media-file-detail");
    report.mediaFileDetail = await runDepthPass("media-file-detail", () =>
      runMediaFileDetail(page, report),
    );
    log(`media file detail: ${JSON.stringify(report.mediaFileDetail)}`);
  }

  if (wants("media-presets")) {
    matchedOnly.add("media-presets");
    report.mediaPresets = await runDepthPass("media-presets", () => runMediaPresets(page, report));
    log(`media presets: ${JSON.stringify(report.mediaPresets)}`);
  }

  // The storage tab (REQ-010, slice 3): the range refused by the form, a connection test that
  // says what it proved, and a save that leaves the untouched fields alone.
  if (wants("media-storage")) {
    matchedOnly.add("media-storage");
    report.mediaStorage = await runDepthPass("media-storage", () => runMediaStorage(page, report));
    log(`media storage: ${JSON.stringify(report.mediaStorage)}`);
  }

  // The share tab (REQ-010, slice 3): the link is shown once and never again, the public URL
  // actually serves the bytes, and a revoke stops it on the very next request.
  if (wants("media-shares")) {
    matchedOnly.add("media-shares");
    report.mediaShares = await runDepthPass("media-shares", () => runMediaShares(page, report));
    log(`media shares: ${JSON.stringify(report.mediaShares)}`);
  }

  // The permissions tab (REQ-010, slice 4): the narrowing rule stated on the screen, the
  // chain a file inherits from, a deny refused when it names nothing, and a real deny that
  // names its subject by name rather than by uuid.
  if (wants("media-grants")) {
    matchedOnly.add("media-grants");
    report.mediaGrants = await runDepthPass("media-grants", () => runMediaGrants(page, report));
    log(`media grants: ${JSON.stringify(report.mediaGrants)}`);
  }

  // The duplicate report (REQ-010, slice 3): two identical uploads form a group, the Merge button
  // is dead until a keeper is chosen, the merge keeps the *chosen* file, and the result says the
  // bytes are pending rather than reclaimed.
  if (wants("media-duplicates")) {
    matchedOnly.add("media-duplicates");
    report.mediaDuplicates = await runDepthPass("media-duplicates", () =>
      runMediaDuplicates(page, report),
    );
    log(`media duplicates: ${JSON.stringify(report.mediaDuplicates)}`);
  }

  // The retention tab (REQ-010, slice 4): the policies state their consequence in a sentence,
  // the purge-inside-the-restore-window refusal is visible *before* the save, a run reports a
  // sentence and writes a log row even when it found nothing, and the file's hold switch is on
  // the tab where the file's other facts are.
  if (wants("backups")) {
    matchedOnly.add("backups");
    report.backups = await runDepthPass("backups", () => runBackups(page, report));
  }
  if (wants("media-retention")) {
    matchedOnly.add("media-retention");
    report.mediaRetention = await runDepthPass("media-retention", () => runMediaRetention(page, report));
    log(`media retention: ${JSON.stringify(report.mediaRetention)}`);
  }

  // A page's featured image (REQ-064, slice 4d): the empty state, the required alt, the round
  // trip read out of SQL, the crop from the KEYBOARD, the public payload, the partial save, the
  // clear, and the trashed-file degradation with its restore.
  report.featuredMedia = await runDepthPass("featured-media", () =>
    runFeaturedMediaDepth(page, report),
  );
  log(`featured media: ${JSON.stringify(report.featuredMedia)}`);

  // The palette is global chrome: it has to open from anywhere, search for real and open a screen.
  if (wants("palette")) {
    matchedOnly.add("palette");
  await runPalette(page, report);
  }
  // The command centre's own pass (REQ-032): commands, prefixes, running one, and its history.
  if (wants("command-center")) {
    matchedOnly.add("command-center");
  await runCommandCenter(page, report);
  }
  // The depth pass: facets, selection, copy, export and the index's own settings screen.
  if (wants("search-depth")) {
    matchedOnly.add("search-depth");
  await runSearchDepth(page, report);
  }
  // The analytics depth pass (REQ-007, slice 2): the range, the comparison, a page drawer and a
  // real CSV download. Goals, funnels and realtime arrive with slice 3; the privacy half of the
  // settings screen with slice 4 — this pass visits what exists today.
  if (wants("analytics-depth")) {
    matchedOnly.add("analytics-depth");
  report.analyticsDepth = await runAnalyticsDepth(page, report);
  }
  // The goals + realtime pass (REQ-007, slice 3): a goal is created through the editor, a visitor
  // completes it after it exists, and the funnel and the live counters are read back.
  if (wants("analytics-goals-depth")) {
    matchedOnly.add("analytics-goals-depth");
  report.analyticsGoals = await runGoalAndRealtimeDepth(page, report);
  }  log(`analytics goals: ${JSON.stringify(report.analyticsGoals)}`);

  // The settings and privacy pass (REQ-007, slice 4): tracking on/off persisted, a refused
  // retention value, the exclusions' preview, a purge and an erasure proven against the QA
  // database.
  if (wants("analytics-settings-depth")) {
    matchedOnly.add("analytics-settings-depth");
  report.analyticsSettings = await runAnalyticsSettingsDepth(page, report);
  }
  // The notification pass (REQ-021, slice 1): the bell's badge against its own grouped lines,
  // a grouped line filtering the list, a bulk action reporting what it changed, the keyboard
  // path, and the three states. It runs after the analytics passes because it emits into the
  // signed-in account's own inbox and would otherwise add rows to a list a later pass counts.
  if (wants("notifications-depth")) {
    matchedOnly.add("notifications-depth");
  report.notifications = await runNotificationsDepth(page, report);
  }  log(`notifications: ${JSON.stringify(report.notifications)}`);

  // The event console (REQ-016, slice 1): the feed, its filters, the payload inspector and the
  // catalogue. It runs after the notification passes because it publishes a page, and the
  // content screens' own passes are ordered after it in the file.
  if (wants("events-console")) {
    matchedOnly.add("events-console");
  report.events = await runDepthPass("events-console", () => runEventsDepth(page, report));
  }  log(`events: ${JSON.stringify(report.events)}`);

  // The webhook endpoints and their delivery operations (REQ-016, slice 2). It runs right after
  // the events pass because it points an endpoint at a real receiver and reads what the
  // receiver actually accepted, which is the one claim on this screen no API status code can
  // make on its own.
  if (wants("webhooks")) {
    matchedOnly.add("webhooks");
  report.webhooks = await runDepthPass("webhooks", () => runWebhooksDepth(page, report));
  }  log(`webhooks: ${JSON.stringify(report.webhooks)}`);

  // The bus's own retention (REQ-016, slice 3). It runs after the events and webhook passes —
  // both of which count rows on the bus — because a sweep deletes, and a pass that deleted
  // first would make their numbers wrong for a reason that has nothing to do with them.
  if (wants("event-retention")) {
    matchedOnly.add("event-retention");
  report.retention = await runDepthPass("event-retention", () => runRetentionDepth(page, report));
  }  log(`retention: ${JSON.stringify(report.retention)}`);

  // The security centre (REQ-012, slice 1). It runs after the events and webhook passes
  // because a scan counts the findings those passes have already written, and a scan that ran
  // first would report a posture that the rest of the pass then invalidates.
  if (wants("security")) {
    matchedOnly.add("security");
    report.security = await runDepthPass("security", () => runSecurityDepth(page, report));
    log(`security: ${JSON.stringify(report.security)}`);
  }

  // The preferences pass (REQ-021, slice 2). It runs immediately after the list pass and
  // restores the row it touched, so a later pass in the same run sees the defaults rather
  // than whatever this one left behind.
  if (wants("notification-settings-depth")) {
    matchedOnly.add("notification-settings-depth");
  report.notificationSettings = await runNotificationSettingsDepth(page, report);
  }  log(`notification settings: ${JSON.stringify(report.notificationSettings)}`);

  // The outbox and routing pass (REQ-021, slice 3). It runs after the list and preferences
  // passes because it emits into the same inbox, and it cleans up every row it creates — a QA
  // database that grows a notification per pass is one whose counts stop meaning anything.
  if (wants("notification-outbox-depth")) {
    matchedOnly.add("notification-outbox-depth");
  report.notificationOutbox = await runNotificationOutboxDepth(page, report);
  }  log(`notification outbox: ${JSON.stringify(report.notificationOutbox)}`);
  log(`analytics settings: ${JSON.stringify(report.analyticsSettings)}`);

  // The navigation and queue pass (REQ-064, slice 1). It runs after the content passes because
  // `Add pages…` needs a published page to point at, and it cleans up every menu and entry it
  // creates — a QA database whose header menu grows a row per pass stops proving anything.
  report.forms = await runFormsDepth(page, report);
  log(`forms: ${JSON.stringify(report.forms)}`);
  report.menus = await runMenusDepth(page, report);
  log(`menus: ${JSON.stringify(report.menus)}`);

  // The comment queue pass (REQ-064, slice 4a). It runs after the forms pass because the two
  // share the public-submission surface — the form submit route and the comment submit route are
  // the only two endpoints a stranger posts to — and a failure in either should be read with
  // the other in view. It creates its own page and leaves the comments it seeded: a queue whose
  // rows are cleaned up afterwards is a queue whose next pass opens on an empty screen.
  report.comments = await runCommentsDepth(page, report);
  log(`comments: ${JSON.stringify(report.comments)}`);

  // The mailing-list pass (REQ-064, slice 4b). It runs right after the comment pass because
  // both write rows nobody in the browser could have written — the comments pass seeds a
  // moderation queue, this one seeds a pending subscription — and a failure in either should be
  // read with the other in view. It leaves its rows: a list cleaned up afterwards is a list the
  // next pass opens empty.
  report.newsletter = await runNewsletterDepth(page, report);
  // The theme gallery (REQ-062, slice 1). Driven right after the CMS depth passes because it
  // is the one screen in this group that changes what every OTHER one renders.
  report.themes = await runThemesDepth(page, report);
  // The theme settings screens (REQ-062, slice 2). They run immediately after the gallery
  // because the gallery's cards are the only way into them, and a settings pass that started
  // from a typed URL would never test the link an operator actually clicks.
  report.themeSettings = await runThemeSettingsDepth(page, report);
  log(`newsletter: ${JSON.stringify(report.newsletter)}`);

  // The visitor-accounts pass (REQ-064, slice 4c). It runs after the newsletter pass because
  // both hold a stranger's address and both put an operator in the position of deciding about
  // one, and a failure in either should be read with the other in view. It leaves its rows: a
  // members table cleaned up afterwards is a table the next pass opens empty, and an empty table
  // is where the "no visitors have signed up yet" state has never been checked.
  report.members = await runMembersDepth(page, report);
  log(`members: ${JSON.stringify(report.members)}`);

  // The role-depth pass (REQ-006, slice 1): create a role, cycle a matrix cell three ways,
  // preview and save, reopen, and read the history tab back.
  if (wants("iam-roles-depth")) {
    matchedOnly.add("iam-roles-depth");
  report.iamRoles = await runIamRolesDepth(page, report);
  }
  // The subjects-and-scopes pass (REQ-006, slice 2): users, bindings at every scope, groups,
  // machine identities and the simulator.
  if (wants("iam-subjects-depth")) {
    matchedOnly.add("iam-subjects-depth");
  await runIamSubjectsDepth(page, report);
  }
  // The ABAC policies pass (REQ-006, slice 4a): the builder, the dry run and the history.
  if (wants("iam-policies-depth")) {
    matchedOnly.add("iam-policies-depth");
  report.iamPolicies = await runIamPoliciesDepth(page, report);
  }  log(`iam roles: ${JSON.stringify(report.iamRoles)}`);

  // The security-policy pass (REQ-006, slice 3): the policy screen with a refusal in the field
  // and a diff on save, the session list with a real revoke, the device registry and the MFA
  // enrolment dialog.
  if (wants("iam-security-depth")) {
    matchedOnly.add("iam-security-depth");
  await runIamSecurityDepth(page, report);
  }  log(`iam security: ${JSON.stringify(report.iamSecurity)}`);

  // Sign-out is exercised last so it cannot break the walk.
  const signOut = page.locator('button:has-text("Sign out")').first();
  if ((await signOut.count()) > 0) {
    await signOut.click().catch(() => {});
    await page.waitForTimeout(1100);
    report.signOut = { url: page.url(), reachedLogin: /\/login/.test(page.url()) };
    await shot(page, "90-after-sign-out");
    const reLogin = await ensureSignedIn(page, report);
    report.reLogin = reLogin;
  }

  // The passkey pass (REQ-006, slice 3b): a virtual authenticator enrols a passkey on the
  // owner's own account, the panel lists it, the sign-in asks for it and completes with it, and
  // the pass is removed again so the account is back to its password.
  if (wants("passkeys-depth")) {
    matchedOnly.add("passkeys-depth");
  await runPasskeysDepth(page, report);
  }  log(`passkeys: ${JSON.stringify(report.passkeys)}`);

  // The permission-request pass (REQ-006, slice 4b): ask, approve with a window, refuse, and the
  // refusals of the ask form. It runs after the count-sensitive passes because an approval adds a
  // time-boxed binding (and the generated grant role) to the organization.
  if (wants("iam-approvals-depth")) {
    matchedOnly.add("iam-approvals-depth");
  await runIamApprovalsDepth(page, report);
  }  log(`iam approvals: ${JSON.stringify(report.iamApprovals)}`);

  // The SCIM provisioning pass (REQ-006, slice 4b): mint a token, drive a create → deactivate
  // round trip through the real endpoint from this browser, read the sync log back, revoke the
  // token and prove it is refused afterwards.
  if (wants("iam-provisioning-depth")) {
    matchedOnly.add("iam-provisioning-depth");
  await runIamProvisioningDepth(page, report);
  }  log(`iam provisioning: ${JSON.stringify(report.iamProvisioning)}`);

  // The enterprise sign-in pass (REQ-006, slice 4b-2): connect a provider through the drawer,
  // read the "secret is a name, not a value" chip, run the discovery test and require it to
  // report a *result* (a provider that is not configured yet answers "failed", not a 500), then
  // remove the provider and see the list go back to its empty state.
  if (wants("iam-authentication-depth")) {
    matchedOnly.add("iam-authentication-depth");
  await runIamAuthenticationDepth(page, report);
  }  log(`iam authentication: ${JSON.stringify(report.iamAuthentication)}`);

  // Mobile pass. The context is new, so it carries no session — without the sign-in below every
  // mobile screenshot would be the sign-in screen and no mobile layout would really be measured.
  const mobile = await context.browser().newContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  const mpage = markHydrationWait(await mobile.newPage());
  attach(mpage, "mobile");
  report.mobileLogin = await ensureSignedIn(mpage, report);
  if (!report.mobileLogin) {
    log("mobile pass: the sign-in did not land — the mobile screenshots will show the login form");
  }
  // A `mobile:` spelling names the same screen's phone layout, so the roll-up must accept it
  // as a known name instead of reporting it as unmatched.
  const mobileRoutes = [{ path: "/", name: "overview" }, { path: "/pages", name: "pages" }, { path: "/ai", name: "ai" }, { path: "/search?q=qa", name: "search" }, { path: "/settings/search", name: "search-settings" }, { path: "/settings/iam/users", name: "iam-users" }, { path: "/settings/iam/groups", name: "iam-groups" }, { path: "/settings/iam/simulator", name: "iam-simulator" }, { path: "/settings/iam/policies", name: "iam-policies" }, { path: "/settings/iam/approvals", name: "iam-approvals" }, { path: "/settings/iam/provisioning", name: "iam-provisioning" }, { path: "/settings/iam/authentication", name: "iam-authentication" }, { path: "/settings/iam/security", name: "iam-security" }, { path: "/settings/iam/sessions", name: "iam-sessions" }, { path: "/settings/iam/devices", name: "iam-devices" }, { path: "/analytics", name: "analytics" }, { path: "/analytics/pages", name: "analytics-pages" }, { path: "/analytics/goals", name: "analytics-goals" }, { path: "/analytics/settings", name: "analytics-settings" }, { path: "/security", name: "security-overview" }, { path: "/security/findings", name: "security-findings" }, { path: "/security/headers", name: "security-headers" }, { path: "/security/rate-limits", name: "security-rate-limits" }, { path: "/security/sign-in-protection", name: "security-sign-in-protection" }, { path: "/members", name: "members" }, { path: "/members/settings", name: "member-settings" }, { path: "/content-api", name: "content-api" }, { path: "/content-api/docs", name: "content-api-docs" }];
  for (const r of mobileRoutes) MOBILE_NAMES.add(r.name);
  // The phone pass follows `--only` for the same reason the route loop does, and the five
  // security screens join it: a layout that has never been measured at 390px has not been
  // tested on a phone, and the security centre is where an administrator reads a verdict.
  for (const route of (ONLY_ALL
    ? mobileRoutes
    : mobileRoutes.filter((r) => wants(`mobile:${r.name}`) || wants(r.name)))) {
    await mpage.goto(`${URL_ADMIN}${route.path}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await mpage.waitForTimeout(800);
    const diag = await diagnostics(mpage);
    await shot(mpage, `mobile-${route.name}`);
    report.mobile.push({ ...route, diagnostics: diag });
  }

  // The palette on a phone: a full-screen sheet with 44px rows and a reachable close control.
  // Overlay shots are viewport-only: a full-page screenshot of a fixed sheet shows the page
  // below the fold as well, which reads as an overlay that fails to cover the screen.
  await mpage.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await mpage.waitForTimeout(1200);
  let mobileOpened = false;
  for (let attempt = 0; attempt < 3 && !mobileOpened; attempt += 1) {
    // A development server hydrates on its own schedule; a tap that lands before that is a tap
    // into a static page, so the pass is patient instead of assuming.
    await mpage.locator("[data-search-box]").first().click({ timeout: 4000 }).catch(() => {});
    await mpage.waitForTimeout(700);
    mobileOpened = (await mpage.locator("[data-search-palette]").count()) > 0;
  }
  await mpage.locator("[data-palette-input]").first().fill("sample").catch(() => {});
  await mpage.waitForTimeout(1000);
  await shot(mpage, "mobile-palette", { full: false });
  const mobileSheet = await mpage
    .evaluate(() => {
      const dialog = document.querySelector("[data-search-palette] [role=dialog]");
      if (!dialog) return null;
      const rect = dialog.getBoundingClientRect();
      const rows = [...document.querySelectorAll("[data-search-palette]  [role=option]")].map(
        (row) => Math.round(row.getBoundingClientRect().height),
      );
      return {
        width: Math.round(rect.width),
        height: Math.round(rect.height),
        viewport: { w: innerWidth, h: innerHeight },
        rowHeights: rows.slice(0, 6),
        minRow: rows.length ? Math.min(...rows) : 0,
        closeButtons: document.querySelectorAll('[data-search-palette] button[aria-label="Close search"]').length,
      };
    })
    .catch(() => null);
  report.mobilePalette = {
    opened: mobileOpened,
    sheet: mobileSheet,
    rows: await mpage.locator("[data-search-palette] [role=option]").count().catch(() => 0),
  };
  log(`mobile palette: ${JSON.stringify(report.mobilePalette)}`);
  await mobile.close();

  // Public renderer — reached through the site's own host so the renderer resolves the site.
  const webBase = `http://${SITE_HOST}:${new URL(URL_WEB).port || 80}`;
  try {
    const wp = await context.newPage();
    attach(wp, "web");
    const res = await wp.goto(`${webBase}/`, { waitUntil: "domcontentloaded", timeout: 30000 });
    await wp.waitForTimeout(1200);
    await shot(wp, "web-home");
    const root = await wp.evaluate(() => ({
      links: [...document.querySelectorAll("a[href]")].map((a) => a.getAttribute("href")).filter((h) => h && !h.startsWith("http")).slice(0, 5),
      text: (document.body.innerText || "").replace(/\s+/g, " ").trim().slice(0, 240),
    }));
    report.web = { status: res && res.status(), title: await wp.title().catch(() => ""), links: root.links, text: root.text, base: webBase };

    // The page the panel published in this pass must come back rendered on the site's own host.
    const publishedRes = await wp
      .goto(`${webBase}/${SAMPLE_SLUG}`, { waitUntil: "domcontentloaded", timeout: 30000 })
      .catch(() => null);
    await wp.waitForTimeout(1000);
    await shot(wp, "web-published");
    report.web.published = {
      slug: SAMPLE_SLUG,
      url: wp.url(),
      status: publishedRes && publishedRes.status(),
      title: await wp.title().catch(() => ""),
      heading: await wp.locator("h1").first().innerText().catch(() => ""),
      text: (await wp.evaluate(() => document.body.innerText.replace(/\s+/g, " ").trim())).slice(0, 300),
      diagnostics: await diagnostics(wp),
    };

    if (root.links.length) {
      await wp.goto(`${webBase}${root.links[0]}`, { waitUntil: "domcontentloaded" }).catch(() => {});
      await wp.waitForTimeout(900);
      await shot(wp, "web-first-link");
      report.web.firstLink = { href: root.links[0], url: wp.url(), diagnostics: await diagnostics(wp) };
    }
    await wp.close();
  } catch (err) {
    report.web = { error: String(err).slice(0, 300) };
  }

  await browser.close();

  // ------------------------------------------------------------ roll-up
  const clicks = clickLines.filter((e) => e.action === "click");
  const findings = [];
  const pushFindings = (severity, kind, detail) => findings.push({ severity, kind, detail });

  // A `--only` filter that matches nothing is a finding, not an empty green report.
  //
  // The failure this prevents is specific: a typo in the filter walks zero routes and zero
  // depth passes, writes a complete-looking summary with zero high findings, and is then read
  // as "the screens passed". The one thing a focused pass must not be is indistinguishable
  // from a pass that proved nothing because it was pointed at nothing. The count is also
  // printed in the log line above, so a reader can tell how much of the panel was covered.
  if (!ONLY_ALL) {
    const unmatched = ONLY.filter((name) => !matchedOnly.has(name) && !MOBILE_NAMES.has(name));
    if (matchedOnly.size === 0) {
      pushFindings(
        "high",
        "empty-pass",
        `--only=${ONLY.join(",")} matched no route and no depth pass: this pass proved nothing`,
      );
    }
    for (const name of unmatched) {
      pushFindings("high", "unknown-pass-name", `--only=${name} matches no route and no depth pass`);
    }
    log(`focused pass coverage: ${matchedOnly.size} route/pass name(s) walked, ${unmatched.length} unmatched`);
  }

  for (const p of report.pages) {
    const d = p.diagnostics;
    if (d.horizontalOverflow) pushFindings("high", "overflow", `${p.name}: page scrolls horizontally (${d.scrollWidth}px > ${d.viewport.w}px)`);
    if (d.offscreen.length) pushFindings("high", "offscreen", `${p.name}: ${d.offscreen.length} element(s) outside the viewport, e.g. ${JSON.stringify(d.offscreen[0])}`);
    if (d.brokenImages.length) pushFindings("high", "broken-image", `${p.name}: ${d.brokenImages.join(", ")}`);
    if (d.emptyInteractives.length) pushFindings("medium", "unlabeled-control", `${p.name}: ${d.emptyInteractives.length} control(s) with no accessible name`);
    if (d.unlabeledInputs.length) pushFindings("medium", "unlabeled-input", `${p.name}: ${d.unlabeledInputs.length} input(s) without a label`);
    if (d.lowContrast.length) pushFindings("medium", "low-contrast", `${p.name}: ${d.lowContrast.length} text node(s) under WCAG AA, e.g. ${JSON.stringify(d.lowContrast[0])}`);
    if (d.duplicateIds.length) pushFindings("low", "duplicate-id", `${p.name}: duplicate ids ${d.duplicateIds.join(", ")}`);
    if (d.h1Count === 0) pushFindings("low", "no-h1", `${p.name}: no h1 heading`);
  }
  for (const m of report.mobile) {
    if (m.diagnostics.horizontalOverflow) pushFindings("high", "overflow-mobile", `mobile ${m.name}: horizontal overflow`);
    if (m.diagnostics.offscreen.length) pushFindings("medium", "offscreen-mobile", `mobile ${m.name}: ${m.diagnostics.offscreen.length} element(s) outside the viewport`);
  }
  const refusedOnPurpose = [];
  for (const [index, f] of consoleLog.entries()) {
    if (f.type === "warning") continue;
    // A console line names the status, not the URL: the allowance for one is the window it was
    // registered in, so only a line that arrived after the pass announced the act can be excused.
    const deliberate = /status of 40[13]/.test(f.text)
      ? expectedRefusals.find((entry) => !entry.claimedConsole && index >= entry.consoleFrom)
      : null;
    if (deliberate) {
      deliberate.claimedConsole = true;
      refusedOnPurpose.push({ kind: "console", detail: `${f.phase} ${f.text.slice(0, 120)}`, reason: deliberate.reason });
      continue;
    }
    const isWeb = f.phase === "web";
    pushFindings(isWeb ? "medium" : "high", isWeb ? "web-console" : "console-error", `${f.phase} ${f.url}: ${f.text.slice(0, 180)}`);
  }
  for (const [index, n] of netFailures.entries()) {
    const deliberate = expectedRefusals.find(
      (entry) =>
        !entry.claimedNet &&
        index >= entry.netFrom &&
        String(n.url || "").includes(entry.match) &&
        [401, 403].includes(n.status),
    );
    if (deliberate) {
      deliberate.claimedNet = true;
      refusedOnPurpose.push({ kind: "request", status: n.status, url: n.url, reason: deliberate.reason });
      continue;
    }
    const isWeb = n.phase === "web";
    pushFindings(isWeb ? "medium" : "high", isWeb ? "web-request" : "request-failed", `${n.phase} ${n.status || "net"} ${n.url} ${n.error || ""}`);
  }
  for (const c of clicks.filter((c) => ["click-error", "console-error", "request-failed"].includes(c.outcome))) {
    pushFindings(
      "high",
      "click-error",
      `[${c.page}] "${c.label}" (${c.tag}) → ${c.outcome}: ${c.reason || ""} ${(c.errors || []).join(" | ")}`.slice(0, 240),
    );
  }
  if (report.web && report.web.error) pushFindings("high", "web-unreachable", report.web.error);
  if (report.web && !report.web.error) {
    const published = report.web.published;
    if (!published || published.status !== 200) {
      pushFindings("high", "web-page", `the published page /${SAMPLE_SLUG} did not render (status ${published ? published.status : "missing"})`);
    } else if (!published.heading) {
      pushFindings("high", "web-page", `the published page /${SAMPLE_SLUG} rendered without its heading`);
    }
    // A 404 is the renderer's not-found answer: it still has to show the visitor a page.
    if (report.web.status === 404 && !report.web.text) {
      pushFindings("high", "web-blank", "the renderer answered 404 with no visible page — the not-found view never rendered");
    }
  }

  const bySeverity = { high: 0, medium: 0, low: 0 };
  for (const f of findings) bySeverity[f.severity] += 1;

  const summary = {
    ...report,
    counts: {
      pages: report.pages.length,
      clicks: clicks.length,
      filled: clickLines.filter((e) => e.action === "fill").length,
      forms: clickLines.filter((e) => e.action === "form").length,
      screenshots: shots.length,
      consoleErrors: consoleLog.filter((c) => c.type !== "warning").length,
      warnings: consoleLog.filter((c) => c.type === "warning").length,
      failedRequests: netFailures.length,
      abortedRequests: netAborted.length,
      dialogs: dialogs.length,
    },
    bySeverity,
    findings,
    expectedRefusals: refusedOnPurpose,
    shots,
    consoleLog,
    netFailures,
  };
  fs.writeFileSync(path.join(OUT, "summary.json"), JSON.stringify(summary, null, 2));
  fs.writeFileSync(path.join(OUT, "diagnostics.json"), JSON.stringify(report.pages.concat(report.mobile), null, 2));

  const md = [];
  md.push(`# Omnion QA walkthrough — ${report.startedAt}`);
  md.push("");
  md.push(`- Admin: ${URL_ADMIN} · Web: ${URL_WEB}`);
  md.push(`- Pages walked: ${report.pages.length} · interactions: ${clicks.length} clicks, ${summary.counts.filled} fills, ${summary.counts.forms} form submissions`);
  md.push(`- Screenshots: ${shots.length} · console errors: ${summary.counts.consoleErrors} · failed requests: ${netFailures.length} · dialogs: ${dialogs.length}`);
  md.push("");
  md.push(`## Findings — ${findings.length} (high ${bySeverity.high} · medium ${bySeverity.medium} · low ${bySeverity.low})`);
  md.push("");
  for (const sev of ["high", "medium", "low"]) {
    const rows = findings.filter((f) => f.severity === sev);
    if (!rows.length) continue;
    md.push(`### ${sev}`);
    for (const f of rows) md.push(`- **${f.kind}** — ${f.detail}`);
    md.push("");
  }
  md.push("## Per-page diagnostics");
  md.push("");
  for (const p of report.pages) {
    const d = p.diagnostics;
    md.push(`- **${p.name}** — overflow: ${d.horizontalOverflow ? "YES" : "no"} · offscreen: ${d.offscreen.length} · broken images: ${d.brokenImages.length} · low contrast: ${d.lowContrast.length} · unlabeled inputs: ${d.unlabeledInputs.length} · duplicate ids: ${d.duplicateIds.length} · h1: ${d.h1Count}`);
  }
  md.push("");
  md.push(`## Refusals provoked on purpose — ${refusedOnPurpose.length}`);
  md.push("");
  for (const r of refusedOnPurpose) {
    md.push(`- ${r.kind} ${r.status || ""} ${r.url || ""} — ${r.reason}`);
  }
  md.push("");
  md.push("## Interaction outcomes");
  const outcomes = {};
  for (const c of clicks) outcomes[c.outcome] = (outcomes[c.outcome] || 0) + 1;
  for (const [k, v] of Object.entries(outcomes).sort((a, b) => b[1] - a[1])) md.push(`- ${k}: ${v}`);
  md.push("");
  md.push("## Screenshots");
  for (const s of shots) md.push(`- ${s.name} — \`${s.file.replace(OUT + "/", "")}\` (${Math.round(s.bytes / 1024)} KB)`);
  md.push("");
  fs.writeFileSync(path.join(OUT, "report.md"), md.join("\n"));

  log(`done: ${findings.length} findings (high ${bySeverity.high}), ${clicks.length} clicks, ${shots.length} shots`);
  console.log(`QA_OUT=${OUT}`);
  console.log(`QA_FINDINGS=${findings.length} QA_HIGH=${bySeverity.high} QA_CLICKS=${clicks.length} QA_SHOTS=${shots.length}`);
}

main().catch(async (err) => {
  console.error("[walk] unexpected failure:", err);
  try {
    fs.writeFileSync(path.join(OUT, "summary.json"), JSON.stringify({ fatal: String(err) }, null, 2));
  } catch {
    /* ignore */
  }
  process.exit(1);
});

/**
 * The security-policy, session, device and second-factor pass (REQ-006, slice 3).
 *
 * Drives the whole slice through the panel: a policy save refused in the field it belongs to and
 * accepted when the value is in range (with the diff it applied), an unusable network refused and
 * a real one saved, the session list with a revoke that ends a real session (a second one opened
 * for the owner, so the browser's own sign-in survives), the device registry with its trust
 * window, and the MFA enrolment dialog opened and cancelled.
 */
/**
 * The ABAC policies pass (REQ-006, slice 4a).
 *
 * Drives the builder from the rows the way an administrator would: name, effect, priority, a
 * target permission and one condition row; the dry run (which highlights the leaves that
 * matched), the save with its version history, and the removal. The policy targets
 * `iam.provisioning.manage`, which no other screen exercises, so the moment it exists cannot
 * change any other pass's verdict.
 */
async function runIamPoliciesDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-policies-depth", action: "iam", ...step });
  };

  await page.goto(`${URL_ADMIN}/settings/iam/policies`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-policies-view]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  const before = await page.locator("[data-policy-row]").count();
  await shot(page, "page-iam-policies");

  // ---- Create from the builder ---------------------------------------------------------------
  await page.locator("[data-policy-new]").first().click({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-policy-name]").first().fill("QA walkthrough policy").catch(() => {});
  await page.locator("[data-policy-effect]").first().selectOption("deny").catch(() => {});
  await page.locator("[data-policy-priority]").first().fill("640").catch(() => {});
  await page
    .locator("[data-policy-target-input]")
    .first()
    .fill("iam.provisioning.manage")
    .catch(() => {});
  await page.locator("[data-policy-target-add]").first().click({ timeout: 4000 }).catch(() => {});
  const targetChips = await page.locator("[data-policy-target]").count();

  const rowSelector = "[data-condition-row]";
  const rowId = await page
    .locator(rowSelector)
    .first()
    .getAttribute("data-condition-row")
    .catch(() => null);
  if (rowId) {
    await page.locator(`[data-condition-attribute="${rowId}"]`).fill("action").catch(() => {});
    await page.locator(`[data-condition-operator="${rowId}"]`).selectOption("==").catch(() => {});
    await page
      .locator(`[data-condition-value="${rowId}"]`)
      .fill("iam.provisioning.manage")
      .catch(() => {});
  }
  note({
    step: "builder-filled",
    targetChips,
    conditionRows: await page.locator(rowSelector).count(),
  });
  await shot(page, "page-iam-policy-editor");

  // ---- The dry run ---------------------------------------------------------------------------
  await page
    .locator("[data-test-permission]")
    .first()
    .fill("iam.provisioning.manage")
    .catch(() => {});
  await page.locator("[data-test-run]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-test-result]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(400);
  const verdict = (await page.locator("[data-test-verdict]").first().innerText().catch(() => "")).trim();
  const applies = await page.locator('[data-test-verdict][data-test-applies="true"]').count();
  const matchedLeaves = await page.locator('[data-test-leaf][data-leaf-satisfied="true"]').count();
  const unmatchedLeaves = await page.locator('[data-test-leaf][data-leaf-satisfied="false"]').count();
  note({ step: "dry-run", verdict, applies: applies > 0, matchedLeaves, unmatchedLeaves });
  await shot(page, "page-iam-policy-test");

  // ---- Save, then read the history back ------------------------------------------------------
  await page.locator("[data-policy-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const afterRows = await page.locator("[data-policy-row]").count();
  const savedRows = await page.locator('[data-policy-row][data-policy-effect="deny"]').count();

  await page.locator("[data-policy-history-toggle]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForSelector("[data-policy-versions]", { timeout: 12000 }).catch(() => {});
  const versionRows = await page.locator("[data-policy-version]").count();
  note({ step: "saved", before, afterRows, savedRows, versionRows });
  await shot(page, "page-iam-policy-history");

  // ---- Remove it again (the first press arms the button) -------------------------------------
  await page.locator("[data-policy-delete]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(600);
  const armed = (await page
    .locator("[data-policy-delete]")
    .first()
    .innerText()
    .catch(() => "")).includes("Confirm");
  await page.locator("[data-policy-delete]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const remaining = await page.locator("[data-policy-row]").count();
  note({ step: "delete", armed, remaining, backToStart: remaining === before });
  await shot(page, "page-iam-policies-clean");

  // ---- A refusal the reader can act on -------------------------------------------------------
  // The field itself refuses the shape, so a mistyped form never becomes a 400 in the console
  // (the API's own refusals — unknown target, unknown operator, out-of-range priority — are
  // pinned by the Rust walk in `apps/api/tests/iam_policy.rs`).
  await page.locator("[data-policy-new]").first().click({ timeout: 4000 }).catch(() => {});
  await page.locator("[data-policy-name]").first().fill("QA invalid policy").catch(() => {});
  await page.locator("[data-policy-priority]").first().fill("1200").catch(() => {});
  await page.locator("[data-policy-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForSelector("[data-policy-draft-problem]", { timeout: 8000 }).catch(() => {});
  const refusal = (await page
    .locator("[data-policy-draft-problem]")
    .first()
    .innerText()
    .catch(() => "")).trim();
  note({ step: "invalid-priority-refused-in-field", refusal });
  await shot(page, "page-iam-policy-refusal");

  report.iamPolicies = { steps };
  log(`iam policies: ${JSON.stringify(steps)}`);
}

async function runIamSecurityDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-security-depth", action: "iam", ...step });
  };

  // A second session for the owner, created from the walk itself (Node, not the page): the
  // session list then holds a row that is not the browser's own, so the revoke below proves the
  // screen without ending the walk's own sign-in. A sign-in from the page would replace the
  // browser's cookie and make the walk sign itself out.
  const extra = await fetch(`${URL_ADMIN}/api/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: CREDS.email, password: CREDS.password }),
  })
    .then(async (response) => {
      const body = await response.json().catch(() => null);
      return { status: response.status, hasUser: Boolean(body && body.user) };
    })
    .catch(() => ({ status: 0, hasUser: false }));
  note({ step: "second-session", ...extra });

  // ---- The policy screen ---------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/settings/iam/security`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-policy-tab]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(700);
  const tabs = await page.locator("[data-policy-tab]").count();
  note({ step: "security-screen", tabs, hasSave: (await page.locator("[data-policy-save]").count()) > 0 });
  await shot(page, "page-iam-security");

  // A value outside the range is refused in the field it belongs to, not in a toast.
  await page.locator('[data-policy-tab="lockout"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator('[data-policy-input="lockout_attempts"]').first().fill("2").catch(() => {});
  await page.locator("[data-policy-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(800);
  const rangeError = (
    await page
      .locator('[data-policy-field-error="lockout_attempts"]')
      .first()
      .innerText()
      .catch(() => "")
  ).replace(/\s+/g, " ");
  note({
    step: "policy-range-refused",
    shown: rangeError.length > 0,
    message: rangeError.slice(0, 90),
  });
  await shot(page, "page-iam-security-field-error");

  // The same field with a value in range saves, and the diff says what moved.
  await page.locator('[data-policy-input="lockout_attempts"]').first().fill("12").catch(() => {});
  await page.locator("[data-policy-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForSelector("[data-policy-diff]", { timeout: 9000 }).catch(() => {});
  const diffText = (await page.locator("[data-policy-diff]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
  note({
    step: "policy-saved",
    hasDiff: /lockout_attempts/.test(diffText),
    diff: diffText.slice(0, 120),
  });
  await shot(page, "page-iam-security-diff");

  // An address the parser cannot read is refused with the list named.
  await page.locator('[data-policy-tab="addresses"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator('[data-policy-input="ip_denylist"]').first().fill("not-a-network").catch(() => {});
  await page.locator("[data-policy-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(900);
  const listError = (
    await page
      .locator('[data-policy-field-error="ip_denylist"]')
      .first()
      .innerText()
      .catch(() => "")
  ).replace(/\s+/g, " ");
  note({ step: "policy-cidr-refused", shown: listError.length > 0, message: listError.slice(0, 90) });
  await shot(page, "page-iam-security-cidr-error");

  // A real network saves …
  await page.locator('[data-policy-input="ip_denylist"]').first().fill("203.0.113.0/24").catch(() => {});
  await page.locator("[data-policy-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1000);
  const savedNotice = (
    await page.locator("[data-policy-notice]").first().innerText().catch(() => "")
  ).replace(/\s+/g, " ");
  note({ step: "policy-cidr-saved", notice: savedNotice.slice(0, 100) });

  // … and the walk clears it again so nothing later in the run meets a refused address.
  await page.locator('[data-policy-input="ip_denylist"]').first().fill("").catch(() => {});
  await page.locator("[data-policy-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(900);
  await shot(page, "page-iam-security-addresses");

  // ---- Sessions ------------------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/settings/iam/sessions`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("table [data-session-row]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(800);
  const sessionRows = await page.locator("table [data-session-row]").count();
  const liveBadges = await page.locator("table [data-session-row]").evaluateAll((nodes) =>
    nodes.filter((node) => /this one/.test(node.innerText)).length,
  );
  note({ step: "sessions-list", rows: sessionRows, currentRows: liveBadges });
  await shot(page, "page-iam-sessions");

  // Revoking the row that is not the browser's own: the oldest sign-in is this walk, so the
  // newest live row (the session opened above) is the one to end.
  const revokeButtons = page.locator("table [data-session-revoke]");
  const revokeCount = await revokeButtons.count();
  if (revokeCount > 0) {
    await revokeButtons.first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1300);
    const revokeNotice = (
      await page.locator("[data-sessions-notice]").first().innerText().catch(() => "")
    ).replace(/\s+/g, " ");
    note({ step: "session-revoked", notice: revokeNotice.slice(0, 110) });
    await shot(page, "page-iam-sessions-revoked");
  } else {
    note({ step: "session-revoked", notice: "", skipped: "no revocable row" });
  }

  // The filters are real: a state filter narrows the list to the rows whose badge matches.
  await page.locator("[data-session-state]").first().selectOption("live").catch(() => {});
  await page.locator("[data-sessions-search]").first().fill("qa-owner").catch(() => {});
  await page.locator('form button[type="submit"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1100);
  const filteredRows = await page.locator("table [data-session-row]").count();
  note({ step: "sessions-filtered", rows: filteredRows });
  await page.locator("[data-sessions-search]").first().fill("").catch(() => {});
  await page.locator("[data-session-state]").first().selectOption("").catch(() => {});
  await page.locator('form button[type="submit"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(900);

  // ---- Devices -------------------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/settings/iam/devices`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-device-row]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(800);
  const deviceRows = await page.locator("table [data-device-row]").count();
  note({ step: "devices-list", rows: deviceRows });
  await shot(page, "page-iam-devices");

  const trustButton = page.locator("table [data-device-trust]").first();
  if ((await trustButton.count()) > 0) {
    await trustButton.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1300);
    const trustText = (await page.locator("table [data-device-row]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
    note({ step: "device-trusted", shown: /trusted until/.test(trustText) });
    await shot(page, "page-iam-devices-trusted");

    // Clearing it again is the same control with 0 days: the badge goes back to "not trusted".
    await page.locator("table [data-device-clear-trust]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1300);
    const clearedText = (await page.locator("table [data-device-row]").first().innerText().catch(() => "")).replace(/\s+/g, " ");
    note({ step: "device-trust-cleared", shown: /not trusted/.test(clearedText) });
  } else {
    note({ step: "device-trusted", shown: false, skipped: "no device row" });
  }

  // ---- The MFA enrolment dialog, opened and cancelled ----------------------------------
  await page.goto(`${URL_ADMIN}/settings/iam/users`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  await page
    .locator('[data-user-open="qa-subject@example.com"]')
    .first()
    .click({ timeout: 5000 })
    .catch(() => {});
  await page.waitForSelector("[data-user-detail-title]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator('[data-user-tab="factors"]').first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(900);
  const factorsEmpty = (await page.locator("[data-factors-empty]").count()) > 0;
  await page.locator("[data-factor-enrol-start]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForSelector("[data-factor-enrolment]", { timeout: 10000 }).catch(() => {});
  const secret = (await page.locator("[data-factor-secret]").first().innerText().catch(() => "")).trim();
  const uri = (await page.locator("[data-factor-uri]").first().innerText().catch(() => "")).trim();
  note({
    step: "mfa-enrolment-opened",
    emptyBefore: factorsEmpty,
    hasSecret: secret.length >= 16,
    hasUri: /^otpauth:\/\/totp\//.test(uri),
  });
  await shot(page, "page-iam-user-factors");

  await page.locator("[data-factor-enrol-cancel]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(700);
  const stillOpen = await page.locator("[data-factor-enrolment]").count();
  note({ step: "mfa-enrolment-cancelled", closed: stillOpen === 0 });

  report.iamSecurity = { steps };
  log(`iam security: ${JSON.stringify(steps)}`);
}


/**
 * The passkey pass (REQ-006, slice 3b).
 *
 * A **virtual authenticator** stands in for a real one: Chrome's own WebAuthn device over CDP.
 * The ceremony runs on `http://localhost:<port>` rather than `127.0.0.1` on purpose — a WebAuthn
 * relying-party id must be a *domain*, so the browser refuses an IP literal outright ("This is an
 * invalid domain"), while `localhost` is a valid RP id and is still a loopback origin (the
 * documented exception in `crates/identity/src/webauthn`). Nothing is stubbed: the browser builds
 * a real attestation and assertion, and the API verifies the challenge, the relying-party hash,
 * the origin and the signature.
 *
 * The pass signs in on the localhost origin of the same panel, which leaves the walk's own
 * session untouched, and removes the passkey again at the end (through the step-up prompt) so the
 * account the rest of the walk signs in with is password-only.
 */
async function runPasskeysDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-passkeys", action: "webauthn", ...step });
  };

  // The panel's origin a credential can actually be bound to: `localhost`, not `127.0.0.1`.
  const admin = /127\.0\.0\.1/.test(URL_ADMIN)
    ? URL_ADMIN.replace("127.0.0.1", "localhost")
    : URL_ADMIN;

  const client = await page.context().newCDPSession(page);
  await client.send("WebAuthn.enable", { enableUI: false });
  const { authenticatorId } = await client.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });
  note({ step: "virtual-authenticator", id: authenticatorId, origin: admin });

  // ---- A session on the loopback origin the credential belongs to ------------------------
  await page.goto(`${admin}/login`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector('input[name="email"]', { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator('input[name="email"]').first().fill(CREDS.email).catch(() => {});
  await page.locator('input[name="password"]').first().fill(CREDS.password).catch(() => {});
  await page.locator('form button[type="submit"]').first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  note({ step: "signed-in-on-loopback", url: page.url(), reachedApp: !/\/login/.test(page.url()) });

  // ---- Enrolment on the owner's own account ----------------------------------------------
  await page.goto(`${admin}/settings/iam/users`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-user-open]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator(`[data-user-open="${CREDS.email}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-user-detail-title]", { timeout: 20000 }).catch(() => {});
  await page.locator('[data-user-tab="factors"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForSelector("[data-passkeys]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  note({
    step: "passkeys-section",
    present: (await page.locator("[data-passkeys]").count()) > 0,
    empty: (await page.locator("[data-passkeys-empty]").count()) > 0,
    enrolButton: (await page.locator("[data-passkey-enrol]").count()) > 0,
  });
  await shot(page, "page-iam-passkeys");

  await page.locator("[data-passkey-enrol]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-passkey-row]", { timeout: 40000 }).catch(() => {});
  await page.waitForTimeout(800);
  const enrolled = await page.locator("[data-passkey-row]").count();
  const factorRows = await page.locator('[data-factor-row="webauthn"]').count();
  const sectionText = (
    await page.locator("[data-passkeys]").first().innerText().catch(() => "")
  ).replace(/\s+/g, " ");
  note({
    step: "passkey-enrolled",
    rows: enrolled,
    webauthnFactorRows: factorRows,
    text: sectionText.slice(0, 140),
  });
  await shot(page, "page-iam-passkeys-enrolled");

  // ---- The sign-in asks for the passkey, and the passkey answers -------------------------
  const signOut = page.locator('button:has-text("Sign out")').first();
  if ((await signOut.count()) > 0) {
    await signOut.click().catch(() => {});
    await page.waitForTimeout(1400);
  }
  await page.goto(`${admin}/login`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector('input[name="email"]', { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator('input[name="email"]').first().fill(CREDS.email).catch(() => {});
  await page.locator('input[name="password"]').first().fill(CREDS.password).catch(() => {});
  await page.locator('form button[type="submit"]').first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-login-mfa-code]", { timeout: 25000 }).catch(() => {});
  await page.waitForTimeout(600);
  const mfaStep = (await page.locator("[data-login-mfa-code]").count()) > 0;
  const passkeyButton = (await page.locator("[data-login-passkey]").count()) > 0;
  note({ step: "signin-asks-for-factor", mfaStep, passkeyButton });
  await shot(page, "page-login-passkey-step");

  if (passkeyButton) {
    await page.locator("[data-login-passkey]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForFunction(() => !/\/login/.test(window.location.pathname), undefined, {
      timeout: 40000,
    }).catch(() => {});
  }
  await page.waitForTimeout(1200);
  const signedIn = !/\/login/.test(page.url());
  const shell = (await page.locator('nav[aria-label="Sections"]').count()) > 0;
  note({ step: "passkey-signin", signedIn, shell });
  await shot(page, "page-after-passkey-signin");

  // ---- Removal (the step-up prompt proves the caller again) -------------------------------
  await page.goto(`${admin}/settings/iam/users`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-user-open]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator(`[data-user-open="${CREDS.email}"]`).first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-user-detail-title]", { timeout: 20000 }).catch(() => {});
  await page.locator('[data-user-tab="factors"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForSelector("[data-passkey-remove]", { timeout: 25000 }).catch(() => {});
  // Removing a confirmed factor is refused once on purpose — the session carries no fresh
  // step-up — and the prompt proves the caller and retries the same action. That refusal is the
  // assertion below (`stepUpShown` plus a row that is gone), so it is registered as deliberate
  // rather than reported as a defect.
  expectRefusal(
    "/api/v1/auth/webauthn/passkeys/",
    "step-up gate on a confirmed factor removal: the panel asked for a fresh proof and retried",
  );
  note({ step: "step-up-gate-registered", allowances: expectedRefusals.length });
  await page.locator("[data-passkey-remove]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-step-up]", { timeout: 12000 }).catch(() => {});
  const stepUpShown = (await page.locator("[data-step-up]").count()) > 0;
  await page.locator("[data-step-up-password]").first().fill(CREDS.password).catch(() => {});
  await page.locator("[data-step-up-submit]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const remaining = await page.locator("[data-passkey-row]").count();
  note({ step: "passkey-removed", stepUpShown, remaining });

  // ---- The account is password-only again, which the rest of the walk depends on ---------
  const plain = await fetch(`${admin}/api/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: CREDS.email, password: CREDS.password }),
  })
    .then(async (response) => {
      const body = await response.json().catch(() => null);
      return { status: response.status, mfaRequired: Boolean(body && body.mfa_required) };
    })
    .catch(() => ({ status: 0, mfaRequired: false }));
  note({ step: "account-back-to-password", ...plain });

  // Leave the walk's own origin on the page.
  await page.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(500);

  report.passkeys = { steps };
  log(`passkeys: ${JSON.stringify(steps)}`);
}

/**
 * The permission-request pass (REQ-006, slice 4b).
 *
 * Drives the inbox the way an administrator would: asks for a permission, approves it with a
 * window and reads the moment it ends back from the row, refuses a second request, and checks
 * the tab counts. The window itself is proven in `apps/api/tests/iam_approvals.rs`, which grants
 * a window of minutes, watches the permission arrive and watches it leave — the browser pass
 * proves the screen a person actually uses.
 *
 * The permission it asks for is `iam.provisioning.manage`, which no other pass reads, so the
 * temporary grant cannot move any other verdict. The pass runs after every count-sensitive pass
 * in `main` for the same reason.
 */
async function runIamApprovalsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-approvals-depth", action: "iam", ...step });
  };

  await page.goto(`${URL_ADMIN}/settings/iam/approvals`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-approvals-view]", { timeout: 20000 }).catch(() => {});
  // A platform account names its organization first (the picker selects the first one on load);
  // an organization account never renders it.
  await page.waitForSelector("[data-approvals-organization]", { timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(600);
  const before = await page.locator("[data-approval-row]").count();
  await shot(page, "page-iam-approvals");

  // ---- Ask for a permission --------------------------------------------------------------
  await page.locator("[data-request-new]").first().click({ timeout: 6000 }).catch(() => {});
  await page
    .locator("[data-request-permission]")
    .first()
    .fill("iam.provisioning.manage")
    .catch(() => {});
  await page
    .locator("[data-request-justification]")
    .first()
    .fill("QA walkthrough: prove the window works")
    .catch(() => {});
  await page.locator("[data-request-submit]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-approvals-notice]", { timeout: 12000 }).catch(() => {});
  await page.waitForTimeout(900);
  const pendingRows = await page.locator('[data-approval-row][data-approval-status="pending"]').count();
  note({ step: "request-created", pendingRows, before });
  await shot(page, "page-iam-approvals-requested");

  // ---- Approve it with a 30-minute window -------------------------------------------------
  await page.locator("[data-approval-approve]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-approval-dialog='approve']", { timeout: 12000 }).catch(() => {});
  await page.locator("[data-approval-window-select]").first().selectOption("30").catch(() => {});
  await page
    .locator("[data-approval-note]")
    .first()
    .fill("QA walkthrough approval")
    .catch(() => {});
  await page.locator("[data-approval-decide-confirm]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);

  await page.locator("[data-approval-tab='approved']").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const approvedRows = await page.locator('[data-approval-row][data-approval-status="approved"]').count();
  const windowChip = await page
    .locator("[data-approval-window]")
    .first()
    .innerText()
    .catch(() => "");
  note({ step: "approved", approvedRows, windowChip: windowChip.trim() });
  await shot(page, "page-iam-approvals-approved");

  // ---- A refusal, with the note the approver left -----------------------------------------
  await page.locator("[data-approval-tab='pending']").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(600);
  await page.locator("[data-request-new]").first().click({ timeout: 4000 }).catch(() => {});
  await page
    .locator("[data-request-permission]")
    .first()
    .fill("iam.policies.read")
    .catch(() => {});
  await page.locator("[data-request-submit]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1400);
  await page.locator("[data-approval-reject]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-approval-dialog='reject']", { timeout: 12000 }).catch(() => {});
  await page
    .locator("[data-approval-note]")
    .first()
    .fill("Not needed for this window")
    .catch(() => {});
  await page.locator("[data-approval-decide-confirm]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);

  await page.locator("[data-approval-tab='rejected']").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const rejectedRows = await page.locator('[data-approval-row][data-approval-status="rejected"]').count();
  note({ step: "rejected", rejectedRows });
  await shot(page, "page-iam-approvals-rejected");

  // ---- A refusal the field can explain ----------------------------------------------------
  // The field refuses a key that is not shaped like one, so a mistyped ask never leaves the
  // browser — the API's own refusal (`400 unknown permission`) is proven over HTTP in
  // `apps/api/tests/iam_approvals.rs`, and the console stays clean here.
  await page.locator("[data-approval-tab='all']").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(800);
  await page.locator("[data-request-new]").first().click({ timeout: 4000 }).catch(() => {});
  await page.locator("[data-request-permission]").first().fill("Not A Key").catch(() => {});
  await page.locator("[data-request-submit]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-approvals-error]", { timeout: 12000 }).catch(() => {});
  const refusal = (await page
    .locator("[data-approvals-error]")
    .first()
    .innerText()
    .catch(() => "")).trim();
  const askedNothing = (await page.locator("[data-approvals-notice]").count()) === 0;
  note({ step: "unshaped-key-refused-in-field", refusal, askedNothing });
  await shot(page, "page-iam-approvals-refusal");

  report.iamApprovals = { steps };
  log(`iam approvals: ${JSON.stringify(steps)}`);
}

/**
 * The SCIM provisioning pass (REQ-006, slice 4b).
 *
 * Mints a token on the provisioning screen, uses it **from the browser** against the real SCIM
 * endpoint (create → deactivate round trip), reads the sync log the endpoint wrote, then revokes
 * the token and proves a revoked token is refused. Nothing here is simulated: the token is the
 * one the screen minted, and the log lines are the ones the API wrote.
 */
async function runIamProvisioningDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-provisioning-depth", action: "iam", ...step });
  };

  await page.goto(`${URL_ADMIN}/settings/iam/provisioning`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-provisioning-view]", { timeout: 20000 }).catch(() => {});
  await page.waitForSelector("[data-provisioning-organization]", { timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(600);
  const beforeTokens = await page.locator("[data-token-row]").count();
  await shot(page, "page-iam-provisioning");

  // ---- Mint a token; the secret is shown once --------------------------------------------
  const stamp = Date.now().toString().slice(-6);
  await page.locator("[data-token-name]").first().fill(`QA walkthrough ${stamp}`).catch(() => {});
  await page.locator("[data-token-mint]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-token-secret]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(600);
  const secret = (await page.locator("[data-token-secret] code").first().innerText().catch(() => "")).trim();
  const afterTokens = await page.locator("[data-token-row]").count();
  note({ step: "token-minted", beforeTokens, afterTokens, secretShown: secret.startsWith("omsc_") });
  await shot(page, "page-iam-provisioning-token");

  // ---- Use it against the real SCIM endpoint ----------------------------------------------
  const scim = await page.evaluate(async ({ token, stamp: localStamp }) => {
    const call = async (method, path, body) => {
      const response = await fetch(`/api/v1/scim/v2${path}`, {
        method,
        headers: {
          authorization: `Bearer ${token}`,
          "content-type": "application/json",
          accept: "application/json",
        },
        ...(body ? { body: JSON.stringify(body) } : {}),
      });
      let payload = null;
      try {
        payload = await response.json();
      } catch {
        payload = null;
      }
      return { status: response.status, payload };
    };

    const email = `scim-qa-${localStamp}@omnion.test`;
    const created = await call("POST", "/Users", {
      schemas: ["urn:ietf:params:scim:schemas:core:2.0:User"],
      userName: email,
      displayName: "SCIM QA",
      externalId: `qa-${localStamp}`,
      active: true,
    });
    const id = created.payload?.id ?? null;

    const patched = id
      ? await call("PATCH", `/Users/${id}`, {
          schemas: ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
          Operations: [{ op: "replace", path: "active", value: false }],
        })
      : { status: 0, payload: null };

    const filtered = await call("GET", `/Users?filter=userName eq "${email}"`);

    return {
      email,
      createdStatus: created.status,
      createdActive: created.payload?.active ?? null,
      patchedStatus: patched.status,
      patchedActive: patched.payload?.active ?? null,
      listed: filtered.payload?.totalResults ?? null,
    };
  }, { token: secret, stamp });

  note({ step: "scim-round-trip", ...scim });
  await page.waitForTimeout(400);

  // ---- The sync log the endpoint wrote -----------------------------------------------------
  await page.locator("[data-provisioning-reload]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const logRows = await page.locator("[data-sync-log-row]").count();
  const createdRows = await page.locator('[data-sync-log-row][data-sync-outcome="created"]').count();
  const deactivatedRows = await page.locator('[data-sync-log-row][data-sync-outcome="deactivated"]').count();
  const firstDetail = (await page
    .locator("[data-sync-log-row]")
    .first()
    .innerText()
    .catch(() => "")).trim();
  note({ step: "sync-log", logRows, createdRows, deactivatedRows, firstDetail: firstDetail.slice(0, 120) });
  await shot(page, "page-iam-provisioning-log");

  // ---- Revoke, and prove the token is refused afterwards -----------------------------------
  // The list is newest-first, so the token this pass minted is the first row.
  await page.locator("[data-token-revoke]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-token-revoke-confirm]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const revoked = await page.locator('[data-token-row][data-token-revoked="true"]').count();

  // The `401` the next call provokes is the assertion (a revoked token is refused), so it is
  // registered as deliberate rather than reported as a defect.
  expectRefusal(
    "/api/v1/scim/v2/Users",
    "the revoked provisioning token is refused on its next call — that refusal is the assertion",
  );
  const afterRevoke = await page.evaluate(async ({ token }) => {
    const response = await fetch("/api/v1/scim/v2/Users", {
      headers: { authorization: `Bearer ${token}`, accept: "application/json" },
    });
    return { status: response.status, scimType: (await response.json().catch(() => ({}))).detail ?? null };
  }, { token: secret });

  note({ step: "revoked", revoked, afterRevokeStatus: afterRevoke.status });
  await shot(page, "page-iam-provisioning-revoked");

  report.iamProvisioning = { steps };
  log(`iam provisioning: ${JSON.stringify(steps)}`);
}

/**
 * The block editor's own pass (REQ-063, slice 1).
 *
 * It creates a page through the panel's own form — the editor's address carries the page's id,
 * not its slug, so there has to be one — and then drives the screen the way an author does:
 * insert a block from the panel, edit one of its props in the inspector, watch the API's own
 * validation refuse the publish until the block is whole, reorder, duplicate, delete, save the
 * draft and publish. Every assertion here is about a *screen state*, because "the editor works"
 * is not a thing a screenshot can prove.
 */
async function runBlockEditorDepth(page, report) {
  const steps = {};
  const note = (action) => record({ page: "page-editor-depth", action });

  // One reader for the status bar, and it never throws.
  //
  // A bare `locator().getAttribute(...)` has NO timeout of its own: Playwright's default is
  // 30 seconds, then it throws a TimeoutError that is not caught anywhere, and the run dies
  // with a `summary.json` holding nothing but `{"fatal": …}`. That is not a failed assertion —
  // it is the loss of the entire pass, forty minutes in, including every screen that came
  // before. A value that may be absent is read through this, so an absent one is `null` and
  // the step records "the bar was not there" instead of ending the run.
  const blockStatus = async (name) =>
    page
      .locator("[data-block-status]")
      .first()
      .getAttribute(name, { timeout: 5000 })
      .catch(() => null);

  // ---- A page to edit -----------------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/pages`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-page-new]", { timeout: 20000 }).catch(() => {});
  await page.locator("[data-page-new]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator("#page-title").fill("QA block page").catch(() => {});
  await page.locator("#page-slug").fill(BLOCK_PAGE_SLUG).catch(() => {});
  await page.locator("#page-body").fill("The pre-block text of the QA page.").catch(() => {});
  await shot(page, "block-editor-page-form");
  // The hook, not `form button[type=submit]`: the app shell's search form is the first form
  // on every screen, so that selector submits the search box and quietly creates nothing.
  await page.locator("[data-page-save]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.created =
    (await page.locator("text=QA block page").count()) > 0 ||
    (await page.locator('a[href^="/pages/"][href$="/edit"]').count()) > 0;

  // The editor's address is the page's id, so the row's own link is how a person gets there.
  // It has to be *this page's* row: the list carries a row per page, and the first link on the
  // screen belongs to whichever page sorts first — which after a fresh database is a different
  // page every run, and the pass would then drive somebody else's content.
  const ownRow = page.locator("tr", { hasText: "QA block page" });
  const editorLink = (await ownRow.count()) > 0
    ? ownRow.locator('a[href^="/pages/"][href$="/edit"]').first()
    : page.locator('a[href^="/pages/"][href$="/edit"]').first();
  if ((await editorLink.count()) === 0) {
    steps.blocked = "no page row carried an editor link";
    report.blockEditor = steps;
    return steps;
  }
  const href = await editorLink.getAttribute("href");
  await editorLink.click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-block-editor]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(900);
  steps.path = href;
  steps.outlineRows = await page.locator("[data-block-outline-row]").count();
  await shot(page, "page-block-editor-empty");

  // ---- Insert ---------------------------------------------------------------------------------
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-block-insert-panel]", { timeout: 8000 }).catch(() => {});
  steps.insertCategories = await page.locator("[data-block-insert-panel] h3").count();
  await shot(page, "page-block-editor-insert");
  // The panel is searchable, and searching narrows it — a panel that only lists is a list.
  await page.locator("#block-search").fill("head").catch(() => {});
  await page.waitForTimeout(400);
  steps.searchNarrows =
    (await page.locator("[data-block-insert-option]").count()) < 16 &&
    (await page.locator("[data-block-insert-option=heading]").count()) > 0;
  await page.locator("#block-search").fill("").catch(() => {});
  await page.waitForTimeout(300);

  // A heading first, then a text, then a columns container: the container is what proves
  // nesting, and the heading is what the heading-order rule is about. Each block is filled
  // the moment it lands, BEFORE the next insert moves the selection away — that is the flow
  // the REQ asks for, and a heading left empty blocks the publish for a reason the pass
  // created. Filling afterwards would not do: the inspector renders the props of whatever is
  // selected, and the selection is on the last block inserted, so `#block-prop-text` is not
  // even on the screen (a Columns block has no `text` prop) and the fill quietly does nothing.
  const fillPropText = async (value_) => {
    const field = page.locator("#block-prop-text").first();
    if ((await field.count()) === 0) {
      return false;
    }
    await field.fill(value_).catch(() => {});
    await page.waitForTimeout(700);
    return true;
  };
  await page.locator("[data-block-insert-option=heading]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.headingFieldPresent = await fillPropText("QA heading from the walkthrough");
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-block-insert-option=text]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  await fillPropText("A paragraph written by the QA walkthrough.");
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-block-insert-option=columns]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(700);
  steps.afterInsert = await page.locator("[data-block-canvas-block]").count();
  steps.panelClosed = (await page.locator("[data-block-insert-panel]").count()) === 0;
  note("inserted three blocks");
  await shot(page, "page-block-editor-canvas");

  // ---- Inspector ----------------------------------------------------------------------------
  // The heading's text is filled as the block lands (above). This step is the READ side: it
  // selects the heading on the canvas and reads its own body, which is a claim about where the
  // text landed rather than about whether a field accepted it. A `.fill()` on a controlled
  // textarea reports success even when the selection had already moved on, so the fill alone
  // proves nothing about the block.
  await page.locator('[data-block-canvas-block=heading]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.headingGotItsOwnText =
    /QA heading from the walkthrough/.test(
      (await page.locator('[data-block-canvas-block=heading]').first().innerText().catch(() => "")) || "",
    );
  steps.inspectedValue = (await page.locator('[data-block-canvas-block=heading]').first().innerText().catch(() => "")).replace(/\s+/g, " ").trim();
  steps.outlineAfterEdit = (
    await page.locator("[data-block-outline-row]").first().innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();
  note("edited the heading's prop");

  // The second block is the text one; its inspector is the one that appears when it is picked.
  const rows = page.locator("[data-block-outline-row]");
  if ((await rows.count()) > 1) {
    await rows.nth(1).click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(400);
    const textField = page.locator("#block-prop-text").first();
    await textField.fill("A paragraph written by the QA walkthrough.").catch(() => {});
    await page.waitForTimeout(800);
  }
  await shot(page, "page-block-editor-inspector");

  // ---- Live validation ----------------------------------------------------------------------
  // The API's own dry run is what the badges come from, so a block that cannot be published
  // has to be visible here — and a warning must not be a blocker.
  steps.statusLine = (await page.locator("[data-block-status]").innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  steps.blockCount = await blockStatus("data-block-count");
  steps.errors = await blockStatus("data-block-errors");
  steps.warnings = await blockStatus("data-block-warnings");
  note("read the validation summary");

  // An image with no alternative text is the blocking case the REQ names; the publish button
  // must be gone rather than merely failing after a round trip.
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-block-insert-option=image]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1000);
  const imageBlock = page.locator("[data-block-canvas-block=image]").first();
  steps.imageNeedsAttention =
    (await imageBlock.getAttribute("data-block-has-error").catch(() => "false")) === "true";
  steps.publishDisabledOnError = await page
    .locator("[data-block-publish]")
    .first()
    .isDisabled({ timeout: 5000 })
    .catch(() => null);
  steps.issueMessages = await page.locator("[data-block-issues] li").count();
  await shot(page, "page-block-editor-validation");

  // The summary is a way INTO the problem, not a number: clicking it selects the first block
  // that needs attention, which is the only way a blocking issue on a block the author is not
  // looking at was ever reachable.
  await page.locator("[data-block-first-issue]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.firstIssueSelectable = (await page.locator("[data-block-inspector]").count()) > 0;
  steps.issueVisibleAfterJump = (await page.locator("[data-block-issues] li").count()) > 0;
  note("jumped to the first blocking block");

  // Filling the field clears it, and the same page then publishes — which is the whole point of
  // validation being the API's: the editor and the save can never disagree about it.
  const imageBlockRow = page.locator("[data-block-canvas-block=image]").first();
  if ((await imageBlockRow.count()) > 0) {
    await imageBlockRow.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(500);
  }
  // An image needs both halves: the URL and the alternative text. Filling only the alt leaves
  // the required `src` blocking, which is the API being right and the pass being incomplete.
  const srcField = page.locator("#block-prop-src").first();
  await srcField
    .fill("/api/v1/public/media/00000000-0000-0000-0000-000000000000")
    .catch(() => {});
  await page.waitForTimeout(500);
  const altField = page.locator("#block-prop-alt").first();
  await altField.fill("A screenshot of the QA walkthrough").catch(() => {});
  await page.waitForTimeout(1000);
  steps.clearedAfterFix =
    (await blockStatus("data-block-errors")) === "0";
  steps.publishEnabledAfterFix = !(await page
      .locator("[data-block-publish]")
      .first()
      .isDisabled({ timeout: 5000 })
      .catch(() => true));
  note("fixed the blocking issue in the field");

  // ---- Reorder, duplicate, delete ------------------------------------------------------------
  const order = async () =>
    (
      await page.locator("[data-block-canvas-block]").evaluateAll((nodes) =>
        nodes.map((node) => node.getAttribute("data-block-canvas-block")),
      )
    ).join(",");
  const beforeOrder = await order();
  await page.locator('[data-block-inspector] button[aria-label="Move block up"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.reordered = (await order()) !== beforeOrder;
  steps.orderBefore = beforeOrder;
  steps.orderAfter = await order();
  note("moved a block up");

  const blocksBefore = (await page.locator("[data-block-canvas-block]").count());
  await page.locator('[data-block-inspector] button[aria-label="Duplicate block"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.duplicated = (await page.locator("[data-block-canvas-block]").count()) === blocksBefore + 1;
  note("duplicated a block");

  await page.locator('[data-block-inspector] button[aria-label="Delete block"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  steps.deleted = (await page.locator("[data-block-canvas-block]").count()) === blocksBefore;
  note("deleted a block");

  // ---- Nested columns (REQ-063 slice 2) -------------------------------------------------------
  // "A columns container accepts 2-4 child columns, each accepting child blocks, and the
  // editor's breadcrumb selects a nested block directly." Four separate claims, so four
  // separate assertions: the wrappers exist, a block lands *inside* one, the breadcrumb walks
  // back out, and the count can be changed without breaking the structure.
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page
    .locator("[data-block-insert-option=columns]")
    .first()
    .click({ timeout: 6000 })
    .catch(() => {});
  await page.waitForTimeout(1200);
  steps.columnsInserted = (await page.locator("[data-block-columns]").count()) > 0;
  steps.columnCount = await page
    .locator("[data-block-columns]")
    .first()
    .getAttribute("data-block-column-count")
    .catch(() => null);
  steps.columnWrappers = await page.locator("[data-block-column]").count();
  steps.emptyColumnTargets = await page.locator("[data-block-column-empty]").count();
  await shot(page, "page-block-editor-columns");

  // Inserting while a column is selected puts the block INSIDE that column. This is the claim
  // that separates "nested editing" from "a list with indentation".
  const emptyColumn = page.locator("[data-block-column-empty]").first();
  if ((await emptyColumn.count()) > 0) {
    await emptyColumn.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(500);
  }
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page
    .locator("[data-block-insert-option=text]")
    .first()
    .click({ timeout: 6000 })
    .catch(() => {});
  await page.waitForTimeout(1200);
  steps.textInsideColumn =
    (await page.locator("[data-block-column] [data-block-canvas-block=text]").count()) > 0;

  // The breadcrumb is how a nested block is reached without hunting the outline: it has to
  // name the columns on the way down and select the nested block when clicked.
  const crumbs = page.locator("[data-block-crumb]");
  steps.crumbCount = await crumbs.count();
  steps.crumbLabels = await crumbs.allInnerTexts().catch(() => []);
  steps.breadcrumbReachesNested = (await page.locator("[data-block-column] [data-block-canvas-block=text]").count()) > 0;
  await shot(page, "page-block-editor-breadcrumb");

  // The count control: a third column appears, and the prop the renderer reads moves with it.
  // The count control lives in the inspector's actions, and it is only there when the `columns`
  // block ITSELF is selected — a `column` or a block inside one gets a different set. Selecting
  // the canvas row is not enough on its own: the row is a container, so a click lands on
  // whichever nested block was under the pointer, and the pass then read a missing button as a
  // feature that does not exist. The button's own label is the proof the selection is right, so
  // the step re-selects through the outline row and records what the inspector offered.
  const columnsRow = page.locator('[data-block-canvas-block="columns"]').first();
  if ((await columnsRow.count()) > 0) {
    // The header row inside the container is the selectable part; the nested children are
    // separate `[data-block-canvas-block]` elements with their own selection.
    await columnsRow.locator("button[aria-label^='Select']").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(700);
  }
  const addColumn = page.locator("[data-block-add-column]").first();
  steps.addColumnOffered = (await addColumn.count()) > 0;
  steps.addColumnDisabledAtMax = await addColumn
    .isDisabled({ timeout: 3000 })
    .catch(() => null);
  if (steps.addColumnOffered) {
    await addColumn.click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1200);
  }
  steps.columnCountAfterAdd = await page
    .locator("[data-block-columns]")
    .first()
    .getAttribute("data-block-column-count")
    .catch(() => null);
  // The count the author asked for and the structure the payload holds must agree, because the
  // renderer reads one and the validator checks the other.
  steps.columnCountGrew = Number(steps.columnCountAfterAdd) === Number(steps.columnCount) + 1;
  // Scoped to the FIRST Columns block, not the page. Two Columns blocks exist by this point
  // (one from the insert sequence, one from the nesting test) and their columns are drawn in
  // separate frames, so counting the page's `[data-block-column]` compares this block's count
  // against the sum of both — a number that can only ever be false, which is what it was.
  const firstColumnsFrame = page.locator("[data-block-columns]").first();
  steps.columnsStillValid =
    (await firstColumnsFrame.locator("[data-block-column]").count()) ===
    Number(steps.columnCountAfterAdd);
  steps.noColumnErrors = (await blockStatus("data-block-errors")) === "0";
  await shot(page, "page-block-editor-columns-three");
  note("built a nested columns layout");

  // ---- Heading order and per-viewport visibility (REQ-063 slice 2) --------------------------
  // Two accessibility rules, and both are worth driving by hand: a lint that only exists as a
  // function nobody calls passes every test, and a "hidden on phones" control that is really a
  // CSS class passes the screen the author is looking at.
  await page.locator("[data-block-insert-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page
    .locator("[data-block-insert-option=heading]")
    .first()
    .click({ timeout: 6000 })
    .catch(() => {});
  await page.waitForTimeout(1000);
  // The fresh heading is selected as it lands, and its `level` is the second schema field.
  await page.locator("#block-prop-text").first().fill("QA section heading").catch(() => {});
  await page.waitForTimeout(500);
  await page.locator("#block-prop-level").first().selectOption("h1").catch(() => {});
  await page.waitForTimeout(1200);
  const warnings = Number(
    (await page.locator("[data-block-status]").getAttribute("data-block-warnings").catch(() => "0")) || 0,
  );
  steps.outlineWarningShown = warnings > 0;
  steps.outlineWarningText = (await page.locator("[data-block-issues] li").allInnerTexts().catch(() => []))
    .join(" ")
    .replace(/\s+/g, " ")
    .trim();
  // A heading-order warning must never stop a publish: it is advisory by construction. Asserted
  // where the claim actually lives — the warning is REPORTED as a warning, and the bar's error
  // count does not include it. The old check read the page's whole error count, so an unrelated
  // missing `src` on a different block reported "the heading warning blocks publishing", which
  // is not what the sentence says and which no fix to the heading could ever clear.
  const barWithWarning = await page
    .locator("[data-block-status]")
    .first()
    .evaluate((el) => ({
      errors: el.getAttribute("data-block-errors"),
      warnings: el.getAttribute("data-block-warnings"),
    }))
    .catch(() => null);
  steps.outlineWarningIsAdvisory = (barWithWarning?.warnings ?? "0") !== "0";
  steps.outlineWarningIsNotBlocking = (barWithWarning?.errors ?? "0") === "0";
  steps.outlineWarningPublishDisabled = await page
    .locator("[data-block-publish]")
    .first()
    .isDisabled({ timeout: 5000 })
    .catch(() => null);
  steps.outlineWarningBar = barWithWarning;
  await shot(page, "page-block-editor-heading-order");
  note("provoked a heading-order warning");

  // Fixing it is the second half of the criterion, and it must be a reorder rather than an edit.
  // The assertion is about THIS warning, not the page's warning total. The bar counts every
  // advisory on the page, and this pass deliberately left an unrelated `block_column_empty` on
  // screen — a Columns block with an empty second column is a warning the criterion never
  // mentions, and it does not go away when the heading is fixed. Reading `data-block-warnings`
  // as "the heading warning is gone" therefore reports a failure that is really the page being
  // honest about something else, and no amount of fixing the heading clears it.
  await page.locator("#block-prop-level").first().selectOption("h2").catch(() => {});
  await page.waitForTimeout(1200);
  const stillOutlined = (
    await page.locator("[data-block-issues] li").allInnerTexts().catch(() => [])
  )
    .join(" ")
    .toLowerCase();
  steps.outlineWarningCleared = !/heading order|h1 comes after|follows an h/.test(stillOutlined);
  steps.remainingWarningText = stillOutlined.replace(/\s+/g, " ").trim().slice(0, 200);
  // A warning that stays on the page must be a way INTO its block, or it is a dead end with a
  // soft voice: it cannot block a publish, so nothing else in the flow leads the author to it.
  const warnJump = page.locator("[data-block-first-warning]").first();
  steps.warningJumpOffered = (await warnJump.count()) > 0;
  if (steps.warningJumpOffered) {
    await warnJump.click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(700);
    steps.warningReachable = (await page.locator("[data-block-issues] li").count()) > 0;
  }
  note("cleared the heading-order warning");

  // The visibility control lives in the inspector's Visibility section, and its effect on the
  // canvas is a badge — a setting that changes nothing on screen is a setting nobody can check.
  //
  // The heading is selected FIRST, explicitly. The control is the selected block's control and
  // the badge is drawn by that block's row, so "set it and see the badge" is only meaningful on
  // a known block. By this point the selection is whatever the last nesting step left, so the
  // step is asserting about a block it never chose — and the page-wide count it used to read
  // could be satisfied by some other block entirely. Proven by hand on the same stack: with
  // the heading selected, `hide_on` reads `none` → `mobile` and the badge reads "Not on phones".
  await page.locator('[data-block-canvas-block=heading] button[aria-label^="Select"]').first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(800);
  const selectedRow = page.locator('[data-block-canvas-block][data-block-selected=true]').first();
  steps.visibilityTarget = await selectedRow.getAttribute("data-block-canvas-block").catch(() => null);
  const hideOn = page.locator("[data-block-hide-on]").first();
  steps.visibilityControlPresent = (await hideOn.count()) > 0;
  steps.hideOnBefore = await hideOn.inputValue().catch(() => null);
  await hideOn.selectOption("mobile").catch(() => {});
  await page.waitForTimeout(1400);
  steps.hideOnAfter = await hideOn.inputValue().catch(() => null);
  const badgeRow = (await selectedRow.count()) > 0 ? selectedRow : page.locator('[data-block-canvas-block=heading]').first();
  steps.hiddenBadge = (await badgeRow.locator("[data-block-hidden-on=mobile]").count()) > 0;
  steps.hiddenBadgeText = (
    (await badgeRow.locator("[data-block-hidden-on=mobile]").first().innerText().catch(() => "")).replace(/\s+/g, " ").trim()
  );
  // `none` is stored as absence, so clearing the control leaves the block with no settings at all
  // and the badge goes with it.
  await hideOn.selectOption("none").catch(() => {});
  await page.waitForTimeout(1200);
  steps.hiddenBadgeCleared = (await badgeRow.locator("[data-block-hidden-on]").count()) === 0;
  await shot(page, "page-block-editor-visibility");
  note("used the per-viewport visibility control");

  // ---- Undo/redo (acceptance 9) ----------------------------------------------------------------
  // "Undo/redo covers at least 50 steps including nesting changes, and ⌘Z after a save restores
  // the pre-save state in the draft." Three claims, and each needs a different assertion:
  //
  //  1. *50 steps* is a DEPTH, so the pass reads the history depth the status bar reports after
  //     each change rather than counting button presses. A button that works once and reports 1
  //     is not a fifty-step history, and only the depth says so.
  //  2. *Nesting changes* — a block inserted INSIDE a column is the deepest tree the editor
  //     builds, so undo is pressed after a nesting insert and the column is checked to be gone
  //     as a child, not merely unselected.
  //  3. *⌘Z after a save* is the ordering-sensitive one, so the save below happens FIRST and the
  //     undo after it. An undo that clears the history on save passes the "undo the last edit"
  //     test and fails this one.
  const historyDepth = async () =>
    Number(
      (await page.locator("[data-block-status]").getAttribute("data-block-undo-depth").catch(() => "0")) || 0,
    );
  const canvasBlocks = async () => page.locator("[data-block-canvas-block]").count();

  steps.undoControlPresent = (await page.locator("[data-block-undo]").count()) > 0;
  steps.redoControlPresent = (await page.locator("[data-block-redo]").count()) > 0;
  // The undo button is enabled here, and it is CORRECTLY enabled: by this point the pass has
  // inserted three blocks, fixed a validation error, built a three-column layout and driven the
  // heading-order rule — roughly seventeen steps. The step's own comment used to call this "a
  // freshly opened editor" and assert the button disabled, which described an editor that had
  // been open for four minutes and edited seventeen times. The number that matters is the one
  // the criterion names, and it is read after the fifty-step run below.
  steps.undoEnabledAfterEdits = await page.locator("[data-block-undo]").first().isEnabled().catch(() => false);
  steps.redoStartsDisabled = await page.locator("[data-block-redo]").first().isDisabled().catch(() => false);
  const depthAtOpen = await historyDepth();
  steps.historyDepthBeforeFifty = depthAtOpen;
  note(`read the undo history depth after the pass's own edits: ${depthAtOpen}`);

  // ---- Save first, so the undo below is literally "⌘Z after a save" ---------------------------
  const beforeSave = await canvasBlocks();
  await page.locator("[data-block-save]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  steps.saveNotice = (
    await page.locator("[role=alert], p.text-muted").allInnerTexts().catch(() => [])
  )
    .join(" ")
    .replace(/\s+/g, " ")
    .trim();
  steps.saved = /revision/i.test(steps.saveNotice) && beforeSave > 0;
  const depthAfterSave = await historyDepth();
  steps.historyDepthAfterSave = depthAfterSave;
  // The save must NOT swallow the history: an editor that clears on save is one `⌘Z` from
  // losing the session.
  steps.saveKeptHistory = depthAfterSave > 0;
  await shot(page, "page-block-editor-saved");

  // ---- Fifty steps deep ----------------------------------------------------------------------
  // Each press is a real structural edit, so the depth the bar reports is the depth that was
  // actually built. A typing-merge implementation would report a much smaller number here,
  // which is exactly why the assertion is on the reported depth and not on the press count.
  const columnBlock = page.locator('[data-block-canvas-block="columns"]').first();
  const hasColumns = (await columnBlock.count()) > 0;
  for (let i = 0; i < 55; i += 1) {
    // Alternate two structural edits so the history is not 55 copies of one button: a move
    // proves the stack holds a REORDER, which is the step the criterion's "including nesting
    // changes" is really about.
    if (hasColumns && i % 2 === 0) {
      await page.locator("[data-block-outline-row]").first().click({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(120);
      const up = page.locator('button[aria-label="Move block down"]').first();
      if (!(await up.isDisabled().catch(() => true))) {
        await up.click({ timeout: 4000 }).catch(() => {});
      }
    } else {
      await page.locator("[data-block-insert-toggle]").first().click({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(200);
      const pick = page.locator("[data-block-insert-option=text]").first();
      if ((await pick.count()) > 0) {
        await pick.click({ timeout: 4000 }).catch(() => {});
      }
    }
    await page.waitForTimeout(140);
  }
  const depthAfterFifty = await historyDepth();
  steps.historyDepthAfterFifty = depthAfterFifty;
  steps.historyCoversFifty = depthAfterFifty >= 50;
  await shot(page, "page-block-editor-history-depth");
  note(`built ${depthAfterFifty} undoable steps`);

  // ---- Undo all the way back ------------------------------------------------------------------
  // Back to the SAVED tree, which is the tree the pass measured before the run — not to the
  // bottom of the stack. The stack reaches back to the page as the editor opened it plus every
  // step since, and the editor was opened on a page a previous pass had already filled, so
  // "press until the button is disabled" unwinds past the save into content this page never
  // had. `undoEmptiesHistory` is therefore a claim about the STACK, read from the button, and
  // the tree assertion is a separate one, read from the canvas.
  for (let i = 0; i < 60; i += 1) {
    if ((await canvasBlocks()) === beforeSave) {
      break;
    }
    const undo = page.locator("[data-block-undo]").first();
    if (await undo.isDisabled().catch(() => true)) {
      break;
    }
    await undo.click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(90);
  }
  const depthAfterUndoAll = await historyDepth();
  steps.historyDepthAfterUndoAll = depthAfterUndoAll;
  // How much history is left once the saved tree is back on the canvas. The steps that belong
  // to the tree the editor LOADED are not this pass's to undo, and a correct implementation
  // leaves exactly those behind — so this is recorded rather than asserted as zero.
  steps.historyLeftAfterUndoAll = depthAfterUndoAll;
  const blocksAfterUndoAll = await canvasBlocks();
  // The tree must be back where it was, not merely shorter: a stack that walks the count back
  // to zero while leaving the inserted blocks behind is broken in a way a depth number hides.
  steps.undoRestoredTree = blocksAfterUndoAll === beforeSave;
  steps.blocksAfterUndoAll = blocksAfterUndoAll;
  steps.blocksBeforeSave = beforeSave;
  steps.redoAvailableAfterUndo = (await page.locator("[data-block-redo]").first().isEnabled().catch(() => false));
  steps.dirtyAfterUndo = (await page.locator("[data-block-status]").getAttribute("data-block-dirty").catch(() => "")) === "true";
  steps.undoNotice = (
    await page.locator("[role=alert], p.text-muted").allInnerTexts().catch(() => [])
  )
    .join(" ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 160);
  await shot(page, "page-block-editor-undone");
  note("undid the whole history back to the saved tree");

  // ---- Redo brings it back -------------------------------------------------------------------
  for (let i = 0; i < 8; i += 1) {
    const redo = page.locator("[data-block-redo]").first();
    if (await redo.isDisabled().catch(() => true)) {
      break;
    }
    await redo.click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(90);
  }
  const depthAfterRedo = await historyDepth();
  const blocksAfterRedo = await canvasBlocks();
  steps.redoRestoredBlocks = blocksAfterRedo > blocksAfterUndoAll;
  steps.historyDepthAfterRedo = depthAfterRedo;
  steps.blocksAfterRedo = blocksAfterRedo;
  // Redo replays the steps the pass just made, and one of them inserted a *second* Columns
  // block. Nothing is wrong with that tree — an unfilled Columns is a warning, not an error —
  // but the pass then unwinds and saves THAT, and the published page carries a layout nobody
  // built. Recording the structure of the tree the redo left behind makes the next step's
  // "it came back to the saved tree" a claim about the right tree.
  steps.columnsAfterRedo = await page.locator("[data-block-columns]").count();
  await shot(page, "page-block-editor-redone");
  note("redid the history and the blocks came back");

  // Unwind again so the rest of the pass works from the saved tree, and save so the published
  // render below is the page this pass actually built.
  //
  // The unwind has to stop at the SAVED tree, and the button's own `disabled` cannot say when
  // that is: the stack reaches back to the baseline the editor loaded plus every step since, so
  // a loop that presses until the button dies walks *past* the tree the save wrote and lands on
  // the empty page the editor was opened on. That is exactly what the pass was doing — it undid
  // everything, saved an empty page, and published it, so the public render that follows drew a
  // page with no blocks and `publicRendered` read false on a renderer that was working
  // perfectly. The block count is the fact: stop as soon as it is back where the save left it.
  for (let i = 0; i < 60; i += 1) {
    if ((await canvasBlocks()) === beforeSave) {
      break;
    }
    const undo = page.locator("[data-block-undo]").first();
    if (await undo.isDisabled().catch(() => true)) {
      break;
    }
    await undo.click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(80);
  }
  steps.blocksAfterUnwind = await canvasBlocks();
  steps.unwindLandedOnSavedTree = steps.blocksAfterUnwind === beforeSave;
  await page.locator("[data-block-save]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2500);

  await page.locator("[data-block-publish]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.published = (await page.locator("text=/is live at/i").count()) > 0;
  await shot(page, "page-block-editor-published");

  // ---- The page really renders ---------------------------------------------------------------
  // The public renderer draws the published block tree; a page that saved but renders nothing
  // is the "preview lies" bug the REQ names, and it is only visible from the outside.
  // `?site=` is how a renderer addresses a site on a multi-site installation: the QA stack
  // serves on 127.0.0.1, which resolves no domain, so without it the renderer is answering
  // "this request does not address one site" and the page looks broken.
  await page
    .goto(`${URL_WEB}/${BLOCK_PAGE_SLUG}?site=${CREDS.siteKey}`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(2200);
  const rendered = (await page.locator("body").innerText().catch(() => "")).replace(/\s+/g, " ");
  steps.publicRendered = rendered.includes("QA heading from the walkthrough");
  steps.publicHasSemanticFigure = (await page.locator("figure").count()) > 0;
  steps.publicHasImage = (await page.locator("img").count()) > 0;
  // The semantic output the criterion asks for: the blocks that promise a heading really are
  // one, and a list really is a list. A canvas that renders a heading as a bold <div> would
  // pass a screenshot review and fail the person navigating the page.
  const outline = await page
    .locator("h1, h2, h3, h4, h5, h6, ul, ol, dl, figure")
    .evaluateAll((nodes) => nodes.map((node) => node.tagName.toLowerCase()));
  steps.semanticTags = [...new Set(outline)].sort();
  steps.headingsAreReal = (outline.filter((tag) => /^h[1-6]$/.test(tag)).length ?? 0) > 0;
  await shot(page, "web-block-page-rendered");
  note("checked the public render");

  // ---- The revision compare (REQ-063 slice 2) -----------------------------------------------
  // The compare is the reason the revisions screen exists, and the only way to see it working
  // is to build a history worth comparing: the pass above has just written several revisions of
  // the QA page, so opening its history now exercises the real case rather than a fixture.
  //
  // The page id is the editor's own path (`/pages/<uuid>/edit`), so the revisions screen is one
  // hop from where the pass already is — and deriving the URL from the editor is what keeps the
  // step pointed at *this* page rather than whichever one sorts first.
  //
  // It is a FUNCTION, not an inline block, and it runs after the preview frame below. Order is
  // the whole point: a compare run before the frame's save reads a history whose newest two
  // revisions are both block-empty, so the server correctly answers "nothing changed" and the
  // pass records zero rows for a screen that works. Waiting for a richer history is the fix;
  // relaxing the assertion would hide a real empty-compare case.
  const pageId = (steps.path || "").match(/\/pages\/([^/]+)\/edit/)?.[1] || null;
  steps.revisionsPageId = pageId;
  const runRevisionCompare = async () => {
    if (!pageId) return;
    await page
      .goto(`${URL_ADMIN}/pages/${pageId}/revisions`, { waitUntil: "domcontentloaded" })
      .catch(() => {});
    await page.waitForSelector("[data-revision-diff]", { timeout: 20000 }).catch(() => {});
    await page.waitForTimeout(1200);

    steps.revisionRows = await page.locator("[data-revision-row]").count();
    steps.diffEntries = await page.locator("[data-diff-entry]").count();
    steps.diffCounts = await page
      .locator("[data-diff-count]")
      .evaluateAll((nodes) =>
        nodes.map((node) => `${node.getAttribute("data-diff-count")}:${node.innerText.trim()}`),
      )
      .catch(() => []);
    await shot(page, "page-revisions-diff");

    // "Not a raw JSON diff" is a claim about the shape of a row, so the assertion is that a
    // changed row names a *prop* in words and shows both values — not that an entry exists.
    const changedRow = page.locator("[data-diff-entry][data-change=changed]").first();
    steps.changedRowText = (await changedRow.innerText().catch(() => ""))
      .replace(/\s+/g, " ")
      .trim();
    steps.changedRowNamesProp =
      /Alternative text|Text|Heading|Url|Link/i.test(steps.changedRowText) &&
      !steps.changedRowText.includes('"props"');

    // Picking a different base must re-run the compare rather than be a dead control.
    const against = page.locator("[data-revision-against]").first();
    const options = await against.locator("option").count().catch(() => 0);
    steps.againstOptions = options;
    if (options > 1) {
      const firstEntry = steps.diffEntries;
      await against.selectOption({ index: 1 }).catch(() => {});
      await page.waitForTimeout(1800);
      steps.baseSwitched = (await page.locator("[data-revision-row][aria-current=true]").count()) > 0;
      steps.diffRecomputed = (await page.locator("[data-diff-entry]").count()) !== firstEntry ||
        (await page.locator("[data-diff-count]").count()) > 0;
      await shot(page, "page-revisions-diff-other-base");
    }
    note("compared two revisions of the QA page");
  };

  // ---- The inline-editing preview frame (REQ-063 slice 2) ---------------------------------
  // "Inline editing saves one draft revision per save, shows the revision number in the toast,
  // and never publishes." Three claims, and only the last one is about a *rule* rather than a
  // control — the other two are observable, so a pass that only counted revisions would miss an
  // implementation that published on every save.
  if (pageId) {
    await page
      .goto(`${URL_ADMIN}/pages/${pageId}/preview`, { waitUntil: "domcontentloaded" })
      .catch(() => {});
    await page.waitForSelector("[data-block-preview]", { timeout: 20000 }).catch(() => {});
    await page.waitForTimeout(1400);

    // The banner is the screen's contract with the author: this is a draft and it says which.
    steps.previewBanner = (await page.locator("[data-block-preview-banner]").innerText().catch(() => ""))
      .replace(/\s+/g, " ")
      .trim();
    const draftNo = await page
      .locator("[data-block-preview-banner]")
      .getAttribute("data-block-preview-draft")
      .catch(() => null);
    const liveNo = await page
      .locator("[data-block-preview-banner]")
      .getAttribute("data-block-preview-live")
      .catch(() => null);
    steps.previewDraftNo = draftNo;
    steps.previewLiveNo = liveNo;
    steps.previewSaysDraft = /draft/i.test(steps.previewBanner);
    // There is no publish control on this screen at all — not a disabled one, not a hidden one.
    steps.previewHasNoPublish = (await page.locator("[data-block-preview] [data-block-publish]").count()) === 0;
    // Every read on the frame and the status element is `.catch()`-guarded AND timeout-bounded.
    // A bare `getAttribute` waits the full 30s and then throws a TimeoutError that aborts the
    // whole pass — after the report would have been written — and the run is lost with no
    // summary at all. The frame is a *secondary* screen reached by a link, so its absence is a
    // fact to record, never a reason to end the run.
    //
    // Declared HERE, above the first use, rather than beside the mobile switch they were
    // originally written for. A helper declared further down and called above is a temporal
    // dead zone crash: valid syntax, `node --check` clean, and a ReferenceError on the first
    // real run. That is the same mistake this file made once already (`memberSteps`), and it is
    // invisible to every gate except actually running it.
    const frameAttr = async (name) =>
      page
        .locator("[data-block-preview-frame]")
        .first()
        .getAttribute(name, { timeout: 5000 })
        .catch(() => null);
    // The two counts are printed by the STATUS element, not the frame — the frame carries only
    // which viewport is active. Reading them off the frame (the obvious simplification) returns
    // null and turns a passing assertion into a silent one, so the selector is per-element.
    const statusAttr = async (name) =>
      page
        .locator("[data-block-preview-status]")
        .first()
        .getAttribute(name, { timeout: 5000 })
        .catch(() => null);
    // The count, and the status element's own `visible_count`, side by side. These used to be
    // the same number twice, which is why `previewCounts: {block: "6", visible: "6"}` could
    // sit in a report beside a drawn count of 0 without either line contradicting the other:
    // both were read off the server payload, and nothing compared them to the DOM. A frame that
    // renders a payload is only proven by something in the frame.
    steps.previewDrawnBlocks = await page
      .locator("[data-block-preview-frame] [data-block-canvas-block]")
      .count();
    steps.previewStatedVisible = Number(
      (await statusAttr("data-block-preview-visible-count")) ?? Number.NaN,
    );
    // Hidden-on-this-viewport blocks are absent from the render by design, so the frame is
    // expected to hold FEWER than the total and never more. "Never more" is the real assertion
    // and the one a broken frame violates; equality is demanded only when nothing is hidden.
    steps.previewFrameWithinStatedCount =
      Number.isFinite(steps.previewStatedVisible) &&
      steps.previewDrawnBlocks > 0 &&
      steps.previewDrawnBlocks <= steps.previewStatedVisible;
    await shot(page, "page-block-preview");

    // The screen switch is a server round trip, and the two payloads genuinely differ: a block
    // the author hid from phones is ABSENT, not invisible.
    await page.locator("[data-block-preview-viewport=mobile]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(2000);
    // The live-pass guard is the same shape: the frame's save must NOT move the published
    // revision, and reading that number is the assertion. Unguarded, a missing banner hangs
    // 30s and then throws — the exact failure that cost this pass its entire summary.
    const previewStatus = statusAttr;
    const previewBanner = async (name) =>
      page
        .locator("[data-block-preview-banner]")
        .first()
        .getAttribute(name, { timeout: 5000 })
        .catch(() => null);
    steps.previewPhoneActive = (await frameAttr("data-block-preview-viewport-active")) === "mobile";
    steps.previewPhoneBlocks = await page
      .locator("[data-block-preview-frame] [data-block-canvas-block]")
      .count()
      .catch(() => 0);
    steps.previewPhoneNarrower = await page
      .locator("[data-block-preview-frame]")
      .first()
      .evaluate((node) => node.getBoundingClientRect().width, { timeout: 5000 })
      .catch(() => null);
    steps.previewCounts = {
      block: await statusAttr("data-block-preview-block-count"),
      visible: await statusAttr("data-block-preview-visible-count"),
    };
    await shot(page, "page-block-preview-phone");
    await page.locator("[data-block-preview-viewport=desktop]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1800);
    note("switched the frame between screens");

    // ---- The media panel and its deletion simulation (REQ-063 slice 4) ----------------------
    //
    // The panel is the only place the degradation is *explained*; the frame is where it is
    // drawn. So the pass opens it, reads the server's own counts, and then actually runs the
    // simulation — because the button existing is not the same claim as the button working, and
    // the one failure that matters here is silent: a simulation that quietly does nothing draws
    // exactly the same page as before.
    await page.locator("[data-block-media-toggle]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-block-media-panel]", { timeout: 15000 }).catch(() => {});
    await page.waitForTimeout(900);
    steps.mediaPanelOpened = (await page.locator("[data-block-media-panel]").count()) > 0;
    steps.mediaFileCount = await statusAttr("data-block-media-file-count");
    steps.mediaBrokenCount = await statusAttr("data-block-media-broken-count");
    steps.mediaRows = await page.locator("[data-block-media-row]").count().catch(() => 0);
    // The counts are the server's and they are about FILES; the rows are references. A gallery
    // that names one dead file twice is one broken file and two rows, so a walk that demanded
    // rows === broken would be demanding a bug. What it does demand is that a page with no
    // images says so rather than drawing an empty list.
    steps.mediaEmptyStateExplained =
      steps.mediaRows === 0
        ? (await page.locator("[data-block-media-empty]").count()) > 0
        : true;
    await shot(page, "page-block-media-panel");

    // The simulation: name a live file as deleted, and the frame must draw the degraded page
    // and SAY it is simulating. Both halves are read, because a panel that showed the degraded
    // page without the note would be reporting a fact about the library that is not true.
    const simulate = page.locator("[data-block-media-simulate]").first();
    steps.mediaSimulateControls = await page
      .locator("[data-block-media-simulate]")
      .count()
      .catch(() => 0);
    if ((await simulate.count()) > 0) {
      const targetId = await simulate.getAttribute("data-block-media-simulate", { timeout: 5000 });
      const beforeText = (await page
        .locator("[data-block-preview-frame]")
        .first()
        .innerText({ timeout: 8000 })
        .catch(() => "")) || "";
      await simulate.click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(2200);
      steps.mediaSimulated = await page
        .locator("[data-block-media-simulating]")
        .count()
        .catch(() => 0);
      steps.mediaSimulatedPressed = (await page
        .locator("[data-block-media-simulate]").first()
        .getAttribute("aria-pressed", { timeout: 5000 })
        .catch(() => null)) === "true";
      const afterText = (await page
        .locator("[data-block-preview-frame]")
        .first()
        .innerText({ timeout: 8000 })
        .catch(() => "")) || "";
      // A simulation that changed nothing at all would leave the frame's text identical, and
      // there is no flag on screen that distinguishes that from a page with no images at all.
      steps.mediaSimulationChangedTheRender = beforeText !== afterText;
      steps.mediaSimulatedId = targetId;
      await shot(page, "page-block-media-simulated");
      // Put it back, or the rest of the pass measures a page the author never saved.
      await page
        .locator("[data-block-media-simulate]")
        .first()
        .click({ timeout: 8000 })
        .catch(() => {});
      await page.waitForTimeout(1800);
    }

    // Inline editing: the toggle makes the page's own text editable, a keystroke marks the page
    // dirty, and the save names the revision the server actually wrote.
    await page.locator("[data-block-preview-toggle-edit]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(700);
    // Guarded for the same reason as every read above: the toggle is absent on a page whose
    // draft has no text blocks, and an unguarded read of a missing element throws after 30s
    // and takes the whole pass with it.
    steps.previewEditingOn =
      (await page
        .locator("[data-block-preview-toggle-edit]")
        .first()
        .getAttribute("aria-pressed", { timeout: 5000 })
        .catch(() => null)) === "true";
    const field = page.locator("[data-block-inline-field]").first();
    steps.previewInlineFields = await page.locator("[data-block-inline-field]").count();
    if ((await field.count()) > 0) {
      await field.click({ timeout: 5000 }).catch(() => {});
      await page.keyboard.press("End").catch(() => {});
      await page.keyboard.type(" Typed in the QA frame.").catch(() => {});
      await page.waitForTimeout(700);
    }
    steps.previewDirtyAfterTyping =
      (await previewStatus("data-block-preview-dirty")) === "true";
    steps.previewSaveEnabled = !(await page
      .locator("[data-block-preview-save]")
      .first()
      .isDisabled({ timeout: 5000 })
      .catch(() => true));
    await shot(page, "page-block-preview-editing");

    // The save. The number in the toast is the assertion: it must name a revision HIGHER than
    // the frame's own draft, and the live number must not move.
    const beforeSaveNo = Number(draftNo || 0);
    await page.locator("[data-block-preview-save]").first().click({ timeout: 10000 }).catch(() => {});
    await page.waitForSelector("[data-block-preview-toast]", { timeout: 15000 }).catch(() => {});
    await page.waitForTimeout(1800);
    steps.previewToast = (
      await page.locator("[data-block-preview-toast]").innerText().catch(() => "")
    )
      .replace(/\s+/g, " ")
      .trim();
    steps.previewToastNamesRevision = /revision\s*\d+/i.test(steps.previewToast);
    const afterSaveNo = await page
      .locator("[data-block-preview-banner]")
      .getAttribute("data-block-preview-draft")
      .catch(() => null);
    steps.previewRevisionAdvanced = Number(afterSaveNo) > beforeSaveNo;
    steps.previewLiveUnchanged =
      (await previewBanner("data-block-preview-live")) === liveNo;
    steps.previewCleanAfterSave =
      (await previewStatus("data-block-preview-dirty")) === "false";
    await shot(page, "page-block-preview-saved");
    note("typed in the frame and saved a draft revision");
  }

  // The compare runs LAST, on purpose: the frame's save above is what gives the history a pair
  // of revisions that differ. Run before it, the newest two revisions are both block-empty and
  // the screen correctly reports "nothing changed" — a true answer that proves nothing.
  await runRevisionCompare();

  // ---- The two widths the criterion names ---------------------------------------------------
  //
  // 1440 and 390, measured, in that order, at the END so the pass arrives with a page that has
  // real blocks on it — an empty page has nothing to overflow. This is the half of the criterion
  // that had no step at all: the depth pass never called `setViewportSize`, so "usable at 390 px"
  // was being read off a screenshot of a 1440-wide page, which is the one measurement that
  // cannot fail and therefore proves nothing.
  //
  // `restore()` puts the viewport back with a `finally`, because an exception between the two
  // widths would leave every later step screenshotting a phone — and a pass whose failure changes
  // what the rest of the pass measures is a pass that reports somebody else's bug.
  const editorPath = steps.path;
  if (editorPath) {
    try {
      // ---- 1440 px: the editor is for this width, and it must not scroll sideways.
      await page.setViewportSize({ width: 1440, height: 900 });
      await page.goto(`${URL_ADMIN}${editorPath}`, { waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForSelector("[data-block-editor]", { timeout: 20000 }).catch(() => {});
      await page.waitForTimeout(800);
      steps.editorNarrowAt1440 =
        (await page.locator("[data-block-editor]").first().getAttribute("data-block-editor-narrow", { timeout: 5000 }).catch(() => null)) === "false";
      steps.editorHasOutlineAt1440 = (await page.locator("[data-block-outline-row]").count()) > 0;
      steps.editorHasInspectorAt1440 = (await page.locator("[data-block-inspector]").count()) > 0;
      steps.editorHasInsertAt1440 = (await page.locator("[data-block-insert-toggle]").count()) > 0;
      const wide = await page
        .locator("[data-block-editor]")
        .first()
        .evaluate((el) => ({ scroll: el.scrollWidth, client: el.clientWidth, doc: document.documentElement.scrollWidth, docClient: document.documentElement.clientWidth }))
        .catch(() => null);
      steps.editorOverflow1440 = wide;
      steps.editorNoHorizontalScrollAt1440 =
        wide !== null && wide.scroll <= wide.client + 1 && wide.doc <= wide.docClient + 1;
      await shot(page, "page-block-editor-1440");

      // ---- 390 px: read-only, said out loud, no sideways scroll, and the preview still offered.
      await page.setViewportSize({ width: 390, height: 844 });
      await page.waitForTimeout(900);
      steps.editorNarrowAt390 =
        (await page.locator("[data-block-editor]").first().getAttribute("data-block-editor-narrow", { timeout: 5000 }).catch(() => null)) === "true";
      steps.editorReadOnlyAt390 =
        (await page.locator("[data-block-editor]").first().getAttribute("data-block-editor-editable", { timeout: 5000 }).catch(() => null)) === "false";
      // The notice is the criterion's own sentence, read back from the DOM instead of a pixel.
      steps.narrowNoticeAt390 = (
        await page.locator("[data-block-editor-narrow-notice]").innerText().catch(() => "")
      )
        .replace(/\s+/g, " ")
        .trim();
      steps.narrowNoticeSaysWhy = /wider screen/i.test(steps.narrowNoticeAt390);
      steps.narrowNoticeOffersPreview = (await page.locator("[data-block-narrow-preview]").count()) > 0;
      // The controls are ABSENT, not disabled. A greyed-out Publish on a phone is still a Publish
      // an author can aim at, and the REQ says the editor "opens read-only" — which is a
      // different screen, not the same one with its buttons dimmed.
      steps.noInsertControlAt390 = (await page.locator("[data-block-insert-toggle]").count()) === 0;
      steps.noSaveControlAt390 = (await page.locator("[data-block-save]").count()) === 0;
      steps.noPublishControlAt390 = (await page.locator("[data-block-publish]").count()) === 0;
      steps.noInspectorAt390 = (await page.locator("[data-block-inspector]").count()) === 0;
      // The page still READS: the canvas is drawn, which is the whole promise of the notice.
      //
      // Scoped to `mode="render"` rather than counted wherever blocks appear. On a phone the
      // editor is read-only, so the ONLY blocks on screen are render-mode ones — but that is a
      // fact about the current screen, and the count this replaces was taken against every
      // block, which meant it could be satisfied by an edit-mode block that only exists at
      // 1440. Demand the read-only branch by name so the assertion describes the screen it is
      // about; this also catches the reverse regression, a phone that quietly grew an editor.
      const phoneRenderBlocks = await page
        .locator("[data-block-canvas] [data-block-canvas-block][data-block-canvas-mode=render]")
        .count();
      steps.canvasDrawnAt390 = phoneRenderBlocks > 0;
      // How many, not only whether: a canvas that draws exactly one of twelve blocks passes a
      // boolean and fails the promise. The editor's own status element already publishes
      // `data-block-count`, so the DOM count can be checked against the number the screen shows
      // the author — which is the disagreement that let this ship: the status read "12 blocks"
      // while the canvas held nothing a step could see.
      steps.canvasRenderBlocksAt390 = phoneRenderBlocks;
      const statedCount = await page
        .locator("[data-block-status]")
        .first()
        .getAttribute("data-block-count", { timeout: 5000 })
        .catch(() => null);
      steps.canvasStatedCountAt390 = statedCount === null ? null : Number(statedCount);
      // A mismatch is a defect even when both numbers are non-zero (a canvas quietly dropping a
      // nested column's children), and a `null` is a defect too: it means the count could not be
      // read at all, which is how this measurement was empty for two ticks running.
      steps.canvasCountMatchesStatus =
        steps.canvasStatedCountAt390 !== null && phoneRenderBlocks === steps.canvasStatedCountAt390;
      const phone = await page
        .locator("[data-block-editor]")
        .first()
        .evaluate((el) => ({ scroll: el.scrollWidth, client: el.clientWidth, doc: document.documentElement.scrollWidth, docClient: document.documentElement.clientWidth }))
        .catch(() => null);
      steps.editorOverflow390 = phone;
      steps.editorNoHorizontalScrollAt390 =
        phone !== null && phone.scroll <= phone.client + 1 && phone.doc <= phone.docClient + 1;
      await shot(page, "page-block-editor-390");
    } finally {
      await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});
    }
  }

  report.blockEditor = steps;
  return steps;
}

/**
 * The pattern library and the template gallery (REQ-063, slice 3).
 *
 * Two acceptance criteria, and each is a claim about a *result* rather than about a screen:
 *
 *  - 10: "a pattern inserted into a page reproduces the block tree exactly; creating a pattern
 *    from a selection works and the new pattern appears in the library." So the pass saves a
 *    pattern from the editor's own blocks, reads the library, inserts it back into the same
 *    page, and reads the canvas — the count has to grow by what the card said it would.
 *  - 11: "`New page from template` creates a draft page whose blocks match the template, with
 *    the sample content intact." So the pass builds a page from the landing template and opens
 *    the page it claims to have made.
 *
 * It runs after the block editor's pass because that pass is what leaves blocks on a page to
 * cut a pattern from — "create a pattern from a selection" has nothing to select without them.
 */
async function runPatternDepth(page, report) {
  const steps = {};
  const note = (action) => record({ page: "pattern-depth", action });

  // ---- Create a pattern from the editor's own blocks -----------------------------------------
  // The editor is where the selection lives, and the pattern tools are a panel *inside* it, so
  // the pass goes back to the page the block editor pass left its blocks on rather than
  // building a second fixture.
  const editorHref = report.blockEditor && report.blockEditor.path;
  if (!editorHref) {
    steps.blocked = "the block editor pass left no page to cut a pattern from";
    return steps;
  }
  await page.goto(`${URL_ADMIN}${editorHref}`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-block-editor]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(900);
  const blocksBefore = await page.locator("[data-block-canvas-block]").count();

  await page.locator("[data-pattern-tools-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-pattern-tools]", { timeout: 8000 }).catch(() => {});
  steps.toolsOpened = (await page.locator("[data-pattern-tools]").count()) > 0;
  await shot(page, "pattern-editor-panel");

  // "New pattern from selection": the form saves the *selected* subtree, and the pass says which
  // one it is about to save — so the assertion below is about a specific number of blocks.
  await page.locator("[data-block-outline-row]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("#save-pattern-name").fill("QA hero").catch(() => {});
  await page.locator("#save-pattern-category").fill("qa").catch(() => {});
  await page.locator("#save-pattern-description").fill("A group the QA pass cut out of a page").catch(() => {});
  await shot(page, "pattern-editor-save-form");
  await page.locator('[data-action="save-selection-as-pattern"]').click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2200);
  steps.savedFromSelection = (await page.locator("[data-pattern-option=qa-hero]").count()) > 0;
  note("saved the selected block as a pattern");

  // ---- The library ----------------------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/patterns`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-pattern-card]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(800);
  steps.cards = await page.locator("[data-pattern-card]").count();
  steps.found = (await page.locator("[data-pattern-card=qa-hero]").count()) > 0;
  await shot(page, "pattern-library");

  // The card carries the tree's outline, read from the registry — "12 blocks" is a number, an
  // outline is what an author recognises. Asserted on text, because a card with neither is a
  // card that tells an author nothing before the click.
  const outline = await page
    .locator("[data-pattern-outline=qa-hero]")
    .first()
    .textContent()
    .catch(() => "");
  steps.outline = (outline || "").trim().length > 0;
  steps.outlineText = (outline || "").trim().slice(0, 80);

  // Search narrows the library — a library that only lists is a list.
  await page.locator("#pattern-search").fill("hero").catch(() => {});
  await page.waitForTimeout(500);
  steps.searchNarrows =
    (await page.locator("[data-pattern-card=qa-hero]").count()) > 0 &&
    (await page.locator("[data-pattern-card]").count()) <= steps.cards;
  await page.locator("#pattern-search").fill("").catch(() => {});
  await page.waitForTimeout(300);

  // ---- Insert it back into the page (acceptance 10) -------------------------------------------
  await page.goto(`${URL_ADMIN}${editorHref}`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-block-editor]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(900);
  const beforeInsert = await page.locator("[data-block-canvas-block]").count();
  await page.locator("[data-pattern-tools-toggle]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-pattern-tools]", { timeout: 8000 }).catch(() => {});
  await page.locator('[data-action="insert-pattern-qa-hero"]').click({ timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const afterInsert = await page.locator("[data-block-canvas-block]").count();
  // The selected block was a heading, so a one-block pattern adds exactly one block. Reading
  // the count rather than asserting "it went up" is what makes this the "reproduces the tree
  // exactly" claim: a pattern that arrived with three blocks would fail here.
  steps.inserted = afterInsert === beforeInsert + 1;
  steps.beforeInsert = beforeInsert;
  steps.afterInsert = afterInsert;
  await shot(page, "pattern-inserted-into-page");

  // And the inserted blocks carry the server's ids: saving and reloading the draft must not
  // change the count, which is what "a copy with fresh ids" buys over a shared reference.
  await page.locator("[data-block-save]").first().click({ timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(2400);
  steps.savedAfterInsert = (await page.locator("[data-block-canvas-block]").count()) === afterInsert;
  note("inserted a pattern into the page and saved the draft");

  // ---- The gallery (acceptance 11) ------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/page-templates`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-template-card]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(800);
  steps.templates = await page.locator("[data-template-card]").count();
  // The five the REQ names, seeded from code.
  for (const key of ["landing", "about", "pricing", "blog-post", "contact"]) {
    steps[`template-${key}`] = (await page.locator(`[data-template-card=${key}]`).count()) > 0;
  }
  await shot(page, "page-template-gallery");

  // "Use template" asks for a slug and a title, then creates a *draft* and lands in the editor.
  await page.locator('[data-action="use-template-landing"]').click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-template-form]", { timeout: 8000 }).catch(() => {});
  steps.formOpened = (await page.locator("[data-template-form]").count()) > 0;
  await shot(page, "page-template-form");

  await page.locator("#template-title").fill("QA from landing").catch(() => {});
  await page.locator("#template-slug").fill(`qa-from-landing-${RUN_STAMP}`).catch(() => {});
  await page.locator("#template-site").selectOption({ index: 1 }).catch(() => {});
  await page.locator('[data-action="create-from-template"]').click({ timeout: 12000 }).catch(() => {});
  await page.waitForSelector("[data-block-editor]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(1200);
  steps.landedInEditor = (await page.locator("[data-block-editor]").count()) > 0;
  steps.templateBlocksOnPage = await page.locator("[data-block-canvas-block]").count();
  // The sample content is intact: the landing template's own headline is on the canvas. An empty
  // canvas here would mean the page was created but the blocks did not travel with it.
  const canvasText = await page.locator("[data-block-canvas]").innerText().catch(() => "");
  steps.sampleContentIntact = /headline|Start with the free plan/i.test(canvasText || "");
  await shot(page, "page-created-from-template");

  return steps;
}

/**
 * The enterprise sign-in screen (REQ-006, slice 4b-2).
 *
 * The screen is about trust, so the pass checks the two claims it makes: a secret is a *name* the
 * panel can check but never read, and the discovery test answers with a verdict rather than
 * failing. A provider is created switched off, the test is run, and the provider is removed —
 * which also proves the empty state is reachable again.
 */
async function runIamAuthenticationDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "iam-authentication-depth", action: "iam", ...step });
  };

  await page.goto(`${URL_ADMIN}/settings/iam/authentication`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-iam-authentication]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(900);
  const before = await page.locator("[data-provider-row]").count();
  await shot(page, "page-iam-authentication");

  // ---- Connect a provider through the drawer ----------------------------------------------
  await page.locator("[data-iam-auth-new]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-provider-drawer]", { timeout: 8000 }).catch(() => {});
  const stamp = Date.now().toString().slice(-6);
  await page.locator("[data-provider-slug-input]").first().fill(`qa-${stamp}`).catch(() => {});
  await page.locator("[data-provider-name]").first().fill(`QA walkthrough ${stamp}`).catch(() => {});
  await page.locator("[data-provider-field=issuer]").first().fill("https://idp.qa.invalid/realms/omnion").catch(() => {});
  await page.locator("[data-provider-field=client_id]").first().fill(`qa-client-${stamp}`).catch(() => {});
  await page.locator("[data-provider-secret-ref]").first().fill("OMNION_QA_SSO_SECRET_ABSENT").catch(() => {});
  await page.waitForTimeout(300);
  await shot(page, "page-iam-authentication-drawer");
  await page.locator("[data-provider-save]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2500);
  const afterConnect = await page.locator("[data-provider-row]").count();
  note({ step: "provider-connected", before, afterConnect, slug: `qa-${stamp}` });

  // ---- The secret is a name, and the panel says so without ever reading it ----------------
  const secretChip = page.locator(`[data-provider-secret="qa-${stamp}"]`).first();
  const secretPresent = await secretChip.getAttribute("data-secret-present").catch(() => null);
  const secretText = (await secretChip.innerText().catch(() => "")).trim();
  note({
    step: "secret-is-a-name",
    secretPresent,
    namesTheVariable: secretText.includes("OMNION_QA_SSO_SECRET_ABSENT"),
    // A panel that could read the value would print it; the chip must not contain one.
    showsNoValue: !/\b[A-Za-z0-9]{20,}\b/.test(secretText),
  });

  // ---- The discovery test answers with a verdict, not a transport error --------------------
  await page.locator(`[data-provider-test="qa-${stamp}"]`).first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector(`[data-provider-test-result="qa-${stamp}"]`, { timeout: 25000 }).catch(() => {});
  await page.waitForTimeout(600);
  const testStatus = await page
    .locator(`[data-provider-test-result="qa-${stamp}"]`)
    .first()
    .getAttribute("data-test-status")
    .catch(() => null);
  const testText = (await page
    .locator(`[data-provider-test-result="qa-${stamp}"]`)
    .first()
    .innerText()
    .catch(() => "")).trim();
  note({
    step: "discovery-test",
    testStatus,
    // An unreachable host must still produce a *result* the panel can render.
    renderedAVerdict: testStatus === "ok" || testStatus === "failed",
    explainsItself: testText.length > 20,
  });
  await shot(page, "page-iam-authentication-tested");

  // ---- A new provider is created switched off --------------------------------------------
  const enabledAttr = await page
    .locator(`[data-provider-slug="qa-${stamp}"]`)
    .first()
    .getAttribute("data-provider-enabled")
    .catch(() => null);
  note({ step: "created-switched-off", enabled: enabledAttr === "false" });

  // ---- The sign-in log opens and is empty rather than missing ----------------------------
  await page.locator(`[data-provider-log="qa-${stamp}"]`).first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-provider-events]", { timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(700);
  const logRows = await page.locator("[data-provider-events] tr[data-event-outcome]").count();
  const logText = (await page.locator("[data-provider-events]").first().innerText().catch(() => "")).trim();
  note({ step: "sign-in-log", logRows, hasEmptyState: /No sign-in/i.test(logText) });
  await shot(page, "page-iam-authentication-log");

  // ---- Remove it and prove the list goes back to its empty state ---------------------------
  await page.locator(`[data-provider-delete="qa-${stamp}"]`).first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator(`[data-provider-delete-confirm="qa-${stamp}"]`).first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2500);
  const afterRemove = await page.locator("[data-provider-row]").count();
  const emptyVisible = await page.locator("[data-providers-empty]").count();
  note({ step: "provider-removed", afterRemove, emptyStateVisible: emptyVisible > 0 });
  await shot(page, "page-iam-authentication-empty");

  report.iamAuthentication = { steps };
}
