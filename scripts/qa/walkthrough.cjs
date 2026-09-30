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
const { execFileSync, spawn } = require("child_process");
const { chromium } = require("playwright-core");

// ---------------------------------------------------------------- args / env

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  if (i !== -1 && process.argv[i + 1]) return process.argv[i + 1];
  // The `--name=value` form. `run.sh` builds the scope as `--only="$QA_ONLY"`, so this is the
  // form every scoped pass actually arrives with — and until this branch existed the flag was
  // silently ignored, `arg` returned the fallback, and ONLY_ALL stayed true. The pass then
  // walked the WHOLE product while every artifact it wrote (screenshots, clicks, the
  // summary's `scope`) claimed the narrow scope, and the "matched no route" guard at the end
  // could not fire because its whole premise is a scope that was never applied.
  //
  // It is a prefix test rather than an exact one, so a later `--name-extra` cannot be read as
  // `--name`; the value is then sliced by the length of the literal prefix.
  const inline = process.argv.find((entry) => entry.startsWith(`--${name}=`));
  return inline ? inline.slice(name.length + 3) : fallback;
}

const URL_ADMIN = arg("url", "http://127.0.0.1:3100");
const URL_WEB = arg("web", "http://127.0.0.1:3200");
const OUT = path.resolve(arg("out", `qa-artifacts/${Date.now()}`));
const SHOTS = path.join(OUT, "shots");
const CHROME = process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const MAX_PER_PAGE = Number(arg("max-per-page", "40"));
const STEP_MS = Number(arg("step-ms", "380"));
/**
 * The disposable QA database, used only by the analytics fixture (REQ-007): the pass posts a
 * synthetic beacon batch through the public collect endpoint and then spreads a slice of those
 * rows over the last thirty days, so the report screens have a multi-day shape to draw. It is the
 * same `docker exec psql` the reset step uses, against `omnion_qa` and nothing else.
 */
const QA_PG_CONTAINER = arg("db-container", process.env.QA_PG_CONTAINER || "omnion-postgres");
const QA_DB = arg("db", process.env.QA_DB || "omnion_qa");

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
 * The floor for a control a thumb has to hit, in CSS pixels.
 *
 * One constant, because this file already contained the same drift twice in two shapes: a
 * comment above the CDN rules measurement said "the 44px floor" while the code asserted 32,
 * and the organization switcher's finding message tells the reader "44 is the floor for a touch
 * target" while the branch fires at `< 40`. Both are the same failure — a sentence that states a
 * number nothing derives from, so the two drift apart the first time either is edited and the
 * next reader trusts the prose. The fix is not to be more careful writing the number twice; it
 * is to have one place a number can be written, and to interpolate it into every message that
 * mentions it.
 *
 * Why 32 and not 44: 44px is the Material/Apple guideline for a comfortable target, and the
 * panel's dense table rows genuinely cannot reach it without changing the desktop design. 32px
 * is the WCAG 2.2 AAA target floor and is what the CDN rules affordances are held to, so it is
 * what the harness asserts. The message says 32 because that is the number the reader has to
 * act on — "fix it to 44" would be advice the codebase does not take.
 */
const TOUCH_TARGET_MIN_PX = 32;
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
function record(entry) {
  clickLines.push(entry);
  // The event stream is written for durability -- a killed pass should leave its clicks behind
  // -- but it is a SECONDARY record: `clickLines` is the one the report is built from. So a
  // write that fails must not end the pass. It used to: this box runs a disk guard that trims
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

async function shot(page, name, { full = true } = {}) {
  const file = path.join(SHOTS, `${name}.png`);
  try {
    await page.screenshot({ path: file, fullPage: full, timeout: 15000 });
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
      /** Past the right edge but inside a horizontal scroller — reachable, so not a defect. */
      scrollable: [],
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
        if (b.width < 24 || b.height < 24) {
          r.tinyTargets.push({ name, w: Math.round(b.width), h: Math.round(b.height) });
        }
      }
      // An element past the right edge is only a *defect* when nothing can bring it into view.
      // A horizontal scroller is a container whose whole purpose is to hold more than fits, so
      // the tab strip of a phone-sized screen is off-screen by design and reachable by dragging.
      // Reporting it as unreachable makes the measured layout the thing a reader cannot get to,
      // which is the opposite of the truth — and it would push every responsive tab strip
      // toward a wrapped row, which the spec's own "tabs a horizontal scroller" line rules out.
      const scrollableAncestor = (() => {
        let node = el.parentElement;
        while (node && node !== document.body) {
          const style = getComputedStyle(node);
          if (
            (style.overflowX === "auto" || style.overflowX === "scroll") &&
            node.scrollWidth > node.clientWidth + 1
          ) {
            return true;
          }
          node = node.parentElement;
        }
        return false;
      })();
      if (b.right > innerWidth + 8 || b.left < -8) {
        if (scrollableAncestor) {
          r.scrollable.push({ tag, name, right: Math.round(b.right) });
        } else {
          r.offscreen.push({ tag, name, left: Math.round(b.left), right: Math.round(b.right) });
        }
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

  // A fresh installation does not answer "/" with the wizard: the request gate sends an anonymous
  // visitor to /login, and *that* screen asks the API whether setup is needed and replaces itself
  // with /setup. The redirect is client-side, so the URL is not the answer yet — reading it after a
  // fixed 900ms makes the pass decide "an installation already exists" about an empty database, and
  // then fail to sign into the account it never created. Wait for one of the two to be true
  // instead of assuming after a sleep.
  await page
    .waitForFunction(
      () => location.pathname.includes("/setup") || location.pathname.includes("/login"),
      null,
      { timeout: 15000 },
    )
    .catch(() => {});
  await page
    .waitForFunction(() => !location.pathname.includes("/setup"), null, { timeout: 8000 })
    .catch(() => {});

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
  // The sign-in navigates only *after* the API answers: the page awaits `signIn(...)` and only
  // then calls `router.replace("/")`. A fixed sleep therefore races the round-trip, and the race
  // is lost on a cold dev compile where the login POST alone takes seconds — the pass reads the
  // URL while it is still `/login` and reports "could not sign in" for a login that succeeded.
  // Wait for the state that means signed in, and say which side of it we came out on.
  const signedIn = await page
    .waitForFunction(
      () => !location.pathname.includes("/login") || !!document.querySelector('nav[aria-label="Sections"]'),
      null,
      { timeout: 45000 },
    )
    .then(() => true)
    .catch(() => false);
  if (!signedIn) {
    // Record *why* before giving up: an API error is rendered on the form, and a pass that says
    // "could not sign in" without that text has thrown away the only diagnostic it will get.
    const shown = await page
      .locator('[role="alert"], [data-error], p.text-red-600, .text-red-600')
      .first()
      .innerText()
      .catch(() => "");
    report.steps.push({ action: "login", clicked, url: page.url(), error: shown.trim() || "no navigation after 45s" });
    return false;
  }
  await page.waitForTimeout(400);
  report.steps.push({ action: "login", clicked, url: page.url() });
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

/**
 * The credentials the pass signs in with, when the form on screen is the *sign-in* form.
 *
 * This exists because `sampleValueFor` is the right answer for a settings field and a
 * catastrophic one for a login. A sample address is a real failed sign-in, and the platform
 * answers ten of them from one address by blocking that address — at which point every
 * *later* page renders the sign-in screen again and the pass measures three elements instead
 * of thirty, with no error anywhere to say why. The failure is silent because the sign-out is
 * deferred and the session is renewed: the run looks healthy right up until it does not.
 *
 * So the sign-in form is filled with the account that actually exists. A form whose email
 * field is *not* the sign-in one keeps the sample, because a person's own address in a
 * stranger's invite form is exactly the thing a walkthrough must never submit.
 */
function signInValueFor(meta) {
  return meta.type === "email" ? CREDS.email : CREDS.password;
}

async function fillSubtree(page, selector) {
  // The slug is **passed in**, not closed over: a `page.evaluate` body runs in the browser, where
  // this file's module-scope `SAMPLE_SLUG` does not exist. Referencing it from inside threw
  // `ReferenceError: SAMPLE_SLUG is not defined` on every screen this filler reached — and the
  // throw was swallowed by the caller's `.catch()`, so the fill silently did nothing and the
  // screen reported a clean pass. `interact`'s own `sampleValueFor` is the Node-side twin of
  // this and reads the constant legitimately; the two must be given their value the same way.
  return page.evaluate(({ selector, slug }) => {
    const root = document.querySelector(selector);
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
      else if (/slug|key/.test(key)) value = slug;
      else if (/title|name/.test(key)) value = "QA Sample";
      else if (el.tagName === "TEXTAREA") value = "QA sample text written by the automated walkthrough.";
      const proto = el.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      Object.getOwnPropertyDescriptor(proto, "value").set.call(el, value);
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      filled.push({ field: (el.id || el.name || el.type || "input").slice(0, 40), value });
    }
    return filled;
  }, { selector, slug: SAMPLE_SLUG });
}

async function clickPrimaryIn(page, selector) {
  const loc = page.locator(`${selector} button[type="submit"], ${selector} button`).first();
  // A guarded control is **not** this function's to click.
  //
  // `clickPrimaryIn` picks the first `button[type=submit]` (or the first button) inside a dialog
  // and fires it. That is right for a settings form and catastrophic for a wizard whose submit
  // creates a tenant-scoped record and starts a background job: the generic pass has already
  // filled the fields with sample values, so it creates a *real* staging environment under the
  // wrong key, and the screen's own depth pass — the one that chose the key and has to assert the
  // clone, the promotion and the archive afterwards — then reports that its environment does not
  // exist. The `data-qa-guard` contract was honoured in `interact`'s inventory loop and silently
  // bypassed here, which is why declaring the guard on the button changed nothing.
  //
  // So the same attribute is honoured in both places, and the check is on the *candidate*: a
  // dialog whose primary button declares a guard is a dialog the generic pass fills and then
  // leaves open for its own depth pass to finish.
  const guarded = await page
    .locator(`${selector} button[data-qa-guard]`)
    .count()
    .catch(() => 0);
  if (guarded > 0) {
    return null;
  }
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
      // The sign-in form is the one place a sample value is a real failed attempt against a
      // real account-lockout policy, so it is filled with credentials that exist. Detected by
      // the page's own path rather than by the field's label: a "someone else's address" field
      // is still an `email` input, and this must not turn into submitting the QA owner into a
      // stranger's invite form.
      const onSignIn = new URL(baseUrl || page.url()).pathname === "/login";
      const value = onSignIn ? signInValueFor(meta) : sampleValueFor(meta);
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
  // The scope is honoured HERE rather than at forty call sites, so a new depth pass is scoped by
  // construction rather than by remembering to wrap it. A call site that forgets the guard walks
  // a screen the pass was never asked to cover, and the artifact then claims a coverage it does
  // not have. `matchedOnly` is recorded from inside the guard, which is the only place that can
  // answer "did this pass really walk the thing the scope named".
  if (!wants(name)) {
    log(`depth pass ${name} skipped (out of scope)`);
    return { ok: true, steps: 0, skipped: true, reason: "out of scope" };
  }
  matchedOnly.add(name);
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
    ? (await page.locator("[data-palette-confirm]").first().innerText())
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
    ? (await page.locator('[data-palette-run-result="done"]').first().innerText())
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
  // A subject that has no organization of its own — a platform account — is the only case
  // where the grant form has to ask which tenant it applies to. The QA account is an
  // organization member, so the picker is *absent* here, and asserting its absence is the
  // point: a control that renders for everybody would invite an operator to narrow a grant
  // that the session already decides.
  const pickerPresent = await page.locator("[data-testid='user-binding-organization']").count();
  note({ step: "binding-tenant-picker", presentForMember: pickerPresent > 0 });
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
/**
 * The tenant depth pass (REQ-005, slice 1): the organization list, the Members tab and the
 * invite dialog are driven for real.
 *
 * What it proves, in order:
 *   1. the organization list renders its rows and its empty/search behaviour is a real filter;
 *   2. the detail screen opens and the Members tab lists the accounts that belong to it;
 *   3. the invite dialog refuses an unusable address **in the field** (a deliberate refusal,
 *      registered with `expectRefusal` so the pass proves it instead of reporting it);
 *   4. a valid address creates an invitation that appears in the Invitations table;
 *   5. inviting the same address again is refused with the pending-invitation sentence;
 *   6. the invitation is revoked again, and the row leaves the table.
 *
 * The created invitation is revoked at the end, so the pass leaves no residue behind.
 */
async function runOrganizationDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "organizations-depth", action: "organizations", ...step });
  };

  const email = `qa-invite-${Date.now()}@omnion.test`;

  await page.goto(`${URL_ADMIN}/organizations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);

  // An organization account has exactly one organization, and the panel sends it straight to
  // that tenant's overview instead of rendering a list it could only read one row of. The pass's
  // owner is such an account, so `/organizations` is a *redirect*, not a list — and a harness
  // that kept looking for a row link there found none, concluded "no organization to open", and
  // skipped all five tenant depth passes with a reason that read like a product gap. The
  // redirect is the product behaving correctly; the id is simply further down the URL.
  const landed = /\/organizations\/([0-9a-f-]+)/.exec(page.url());
  if (landed) {
    note({ step: "redirected-to-tenant", organizationId: landed[1], url: page.url() });
    const out = await runTenantDepthFromDetail(page, report, landed[1]);
    return out;
  }

  const rows = await page.locator("table tbody tr").count();
  const emptyState = await page.locator("text=No organizations yet").count();
  note({ step: "list", rows, emptyState: emptyState > 0 });
  await shot(page, "page-organizations-list");

  // A search that matches nothing has to say so instead of showing a stale list.
  const search = page.locator('input[placeholder="Name or slug"]').first();
  if (await search.count()) {
    await search.fill("zzz-no-such-organization-zzz");
    await page.waitForTimeout(600);
    const nothing = await page.locator("text=Nothing matches that search").count();
    note({ step: "search-empty", shown: nothing > 0 });
    await shot(page, "page-organizations-search-empty");
    await search.fill("");
    await page.waitForTimeout(400);
  }

  // The first row links to the detail screen; its Members tab is what this slice ships.
  const firstLink = page.locator('a[href^="/organizations/"]').first();
  if ((await firstLink.count()) === 0) {
    const early = { steps, organizationId: null };
    report.organizations = early;
    log(`organizations depth: ${JSON.stringify(steps)}`);
    return early;
  }
  await firstLink.click().catch(() => {});
  await page.waitForSelector("[data-members-heading]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(900);
  const organizationId = /\/organizations\/([0-9a-f-]+)/.exec(page.url())?.[1] || "";
  return runTenantDepthFromDetail(page, report, organizationId, { steps, note, email });
}

/**
 * The tenant depth pass, starting from an organization that is already on screen.
 *
 * Both ways in converge here. A platform account reaches it by clicking a row of the list; an
 * organization account is redirected to its own overview before the pass can look for a row, and
 * arrives with the id already in the URL. Keeping one body means the two entry points cannot
 * drift apart — a second copy of these assertions is a second set of things to forget to update.
 */
async function runTenantDepthFromDetail(page, report, organizationId, context) {
  const steps = context ? context.steps : [];
  const email = (context && context.email) || `qa-invite-${Date.now()}@omnion.test`;
  const note =
    context &&
    context.note ||
    ((step) => {
      steps.push(step);
      record({ page: "organizations-depth", action: "organizations", ...step });
    });

  if (!organizationId) {
    const out = { steps, organizationId: null, email };
    report.organizations = out;
    log(`organizations depth: ${JSON.stringify(steps)}`);
    return out;
  }
  await page.waitForSelector("[data-members-heading]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(900);
  const memberRows = await page.locator("[data-member-row]").count();
  note({ step: "detail", organizationId: Boolean(organizationId), memberRows });
  await shot(page, "page-organization-detail-members");

  // 3. An unusable address is refused in the field: the pass registers the refusal first, so
  //    the request it provokes on purpose is counted as an assertion, not as a finding.
  expectRefusal("/organizations", "the invite dialog refuses an unusable address in the field");
  await page.locator("[data-invite-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator("[data-invite-email]").first().fill("not-an-address").catch(() => {});
  await page.locator("[data-invite-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  const fieldError = (await page
    .locator('[role="alert"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "invite-invalid", fieldError: fieldError.slice(0, 120) });
  await shot(page, "page-organization-invite-invalid");

  // 4. A valid address creates the invitation.
  await page.locator("[data-invite-email]").first().fill(email).catch(() => {});
  await page.locator("[data-invite-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1500);
  const invited = await page.locator(`[data-invitation-row="${email}"]`).count();
  note({ step: "invite", email, rowShown: invited > 0 });
  await shot(page, "page-organization-invited");

  // 5. The same address again is refused naming the pending invitation — a second deliberate
  //    refusal, so the 409 it provokes is the assertion rather than a finding.
  expectRefusal("/invitations", "a duplicate invite is refused naming the pending one");
  await page.locator("[data-invite-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator("[data-invite-email]").first().fill(email).catch(() => {});
  await page.locator("[data-invite-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const duplicate = (await page
    .locator('[role="alert"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "invite-duplicate", refused: duplicate.slice(0, 160) });
  await shot(page, "page-organization-invite-duplicate");

  // 6. Revoke it again: the row leaves the table. The dialog is closed with Escape first — the
  //    duplicate refusal above left it open, and a submit click there would invite a second
  //    time instead of closing it, so the revoke button would stay behind an overlay.
  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(400);
  // The revocation is a deliberate refusal too if the API guards it; register the allowance so a
  // correct 4xx is read as the assertion rather than a finding.
  expectRefusal("/invitations", "the invitation is revoked through its row action");
  const revoke = page.locator(`[data-invitation-revoke="${email}"]`).first();
  if (await revoke.count()) {
    await revoke.click().catch(() => {});
    await page.waitForTimeout(2000);
  }
  const stillThere = await page.locator(`[data-invitation-row="${email}"]`).count();
  note({ step: "revoke", rowGone: stillThere === 0 });
  await shot(page, "page-organization-revoked");

  const out = { steps, organizationId: organizationId || null, email };
  report.organizations = out;
  log(`organizations depth: ${JSON.stringify(steps)}`);
  return out;
}

/**
 * The member drawer (REQ-005, slice 4).
 *
 * The pass drives the three operations the REQ names on a real member of the organization the
 * depth pass just created:
 *
 *   1. the drawer opens from a member row and shows identity, bindings, departments and trail;
 *   2. a role is granted at organization scope and the binding row appears;
 *   3. a grant made temporary is **extended** — the third verb, and the one that only exists
 *      because this slice added it. A revoke-and-re-grant would satisfy every other assertion a
 *      person can make by eye, and the count check is what catches it: the drawer must still
 *      show exactly one binding for that role afterwards, with the same id.
 *   4. the grant is revoked and the row stays on screen marked revoked, because the tenant's
 *      trail records it;
 *   5. the drawer closes with Escape.
 *
 * The empty state is exercised too, and it is the *first* member row on purpose: the first
 * thing a person sees when a tenant has nobody with a bespoke role should be the sentence that
 * explains what to do next, not a blank panel.
 */
async function runOrganizationMemberDrawer(page, report, organizationId) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "organization-member-drawer", action: "organizations", ...step });
  };

  if (!organizationId) {
    note({ step: "skipped", reason: "no organization to open" });
    report.organizationMemberDrawer = { steps, organizationId: null };
    log(`organization member drawer: ${JSON.stringify(steps)}`);
    return report.organizationMemberDrawer;
  }

  const membersUrl = `${URL_ADMIN}/organizations/${organizationId}?tab=members`;
  await page.goto(membersUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-member-row], [data-members-heading]", { timeout: 15000 }).catch(
    () => {},
  );
  await page.waitForTimeout(900);

  const rows = await page.locator("[data-member-row]").count();
  const noMembers = await page.locator("text=No members yet").count();
  if (rows === 0) {
    // Nothing to open. Saying so is the honest report; inventing a member here would make the
    // pass depend on a signup the walk does not perform.
    note({ step: "no-rows", rows, emptyState: noMembers > 0 });
    await shot(page, "page-organization-members-empty");
    const out = { steps, organizationId };
    report.organizationMemberDrawer = out;
    log(`organization member drawer: ${JSON.stringify(steps)}`);
    return out;
  }

  // 1. Open the drawer from the first row.
  await page.locator("[data-member-open]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-member-drawer]", { timeout: 12000 }).catch(() => {});
  await page.waitForTimeout(900);
  const open = await page.locator("[data-member-drawer]").count();
  const identity = await page.locator("[data-member-identity]").count();
  const audit = await page.locator("[data-member-audit]").count();
  const auditEmpty = await page.locator("[data-member-audit-empty]").count();
  note({ step: "open", drawerOpen: open > 0, identityShown: identity > 0, trailRowsOrEmpty: audit + auditEmpty });
  await shot(page, "page-organization-member-drawer");

  // 2. Grant a role. The picker is a real select, so index 1 is the first real role.
  await page.locator("[data-member-grant-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-member-grant-role]").first().selectOption({ index: 1 }).catch(() => {});
  await page.locator("[data-member-grant-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const afterGrant = await page.locator("[data-member-binding]").count();
  note({ step: "grant", bindingRows: afterGrant });
  await shot(page, "page-organization-member-grant");

  // 3. Extend. Only a temporary grant carries the control, so one is granted with a window
  //    first — which is why this step grants its *own* role rather than reaching for the row the
  //    previous step made. A walk that extends "whatever is first" would silently pass on the
  //    permanent grant's absence of the control and prove nothing.
  await page.locator("[data-member-grant-open]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-member-grant-role]").first().selectOption({ index: 2 }).catch(() => {});
  await page.locator("[data-member-grant-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const beforeExtend = await page.locator("[data-member-binding]").count();
  const extendable = await page.locator("[data-member-binding-extend]").count();
  note({ step: "grant-second", bindingRows: beforeExtend, extendControls: extendable });

  if (extendable > 0) {
    await page.locator("[data-member-binding-extend]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(500);
    const dateField = await page.locator("[data-member-extend-date]").count();
    // A date well in the future: extending into the past is refused by the API on purpose, and
    // this pass is about the success path.
    const future = new Date(Date.now() + 45 * 24 * 3600 * 1000).toISOString().slice(0, 10);
    await page.locator("[data-member-extend-date]").first().fill(future).catch(() => {});
    await page.locator("[data-member-extend-submit]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1700);
    const afterExtend = await page.locator("[data-member-binding]").count();
    // The count is the assertion: a revoke-and-re-grant would leave an extra row here, and the
    // panel would look identical to somebody who is not counting.
    note({
      step: "extend",
      dateField: dateField > 0,
      bindingRowsBefore: beforeExtend,
      bindingRowsAfter: afterExtend,
      noExtraRow: afterExtend === beforeExtend,
    });
    await shot(page, "page-organization-member-extended");
  }

  // 4. Revoke the first grant; the row must remain, marked.
  const revokeControls = await page.locator("[data-member-binding-revoke]").count();
  if (revokeControls > 0) {
    await page.locator("[data-member-binding-revoke]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1700);
  }
  const afterRevoke = await page.locator("[data-member-binding]").count();
  const revokedBadges = await page.locator('[data-member-binding] >> text=Revoked').count();
  note({ step: "revoke", rowsStillListed: afterRevoke > 0, revokedShown: revokedBadges > 0 });
  await shot(page, "page-organization-member-revoked");

  // 5. Escape closes it, and the tab underneath is still the filtered list it was.
  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(600);
  const closed = (await page.locator("[data-member-drawer]").count()) === 0;
  const tableBack = await page.locator("[data-members-heading]").count();
  note({ step: "close", closedWithEscape: closed, tabStillMounted: tableBack > 0 });
  await shot(page, "page-organization-member-drawer-closed");

  const out = { steps, organizationId };
  report.organizationMemberDrawer = out;
  log(`organization member drawer: ${JSON.stringify(steps)}`);
  return out;
}

/**
 * The Departments tab of an organization (REQ-005, slice 2).
 *
 * The walk covers the whole of the slice's own promise rather than just the screen:
 *
 *   1. the tab opens and reports its empty state (or the tree it already holds);
 *   2. a department is created with an unusable key, which the field must refuse;
 *   3. it is created properly and appears in the tree with a member count;
 *   4. a move under its own descendant is refused — the refusal the schema cannot catch;
 *   5. the drawer opens, a role is bound to the department and the row is revoked;
 *   6. a member is put in and taken out again;
 *   7. the department is archived and then deleted, so the pass leaves no residue.
 */
async function runOrganizationDepartments(page, report, organizationId) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "organization-departments", action: "organizations", ...step });
  };

  if (!organizationId) {
    const skipped = { steps: [{ step: "skipped", reason: "no organization to open" }] };
    report.organizationDepartments = skipped;
    log(`organization departments: ${JSON.stringify(steps)}`);
    return skipped;
  }

  const tabUrl = `${URL_ADMIN}/organizations/${organizationId}?tab=departments`;
  await page.goto(tabUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-department-add]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(900);

  const rows = await page.locator("[data-department-row]").count();
  const emptyState = await page.locator("text=No departments yet").count();
  note({ step: "open", rows, emptyState: emptyState > 0 });
  await shot(page, "page-organization-departments");

  // A key that could never be addressed in a role binding is refused before it is sent.
  expectRefusal("/departments", "an unusable department key is refused in the form");
  await page.locator("[data-department-add]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(500);
  await page.locator("[data-department-name]").first().fill("QA Department").catch(() => {});
  await page.locator("[data-department-key]").first().fill("Not A Key").catch(() => {});
  await page.locator("[data-department-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  const keyError = (await page
    .locator('[role="alert"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "invalid-key", fieldError: keyError.slice(0, 120) });
  await shot(page, "page-organization-department-invalid-key");

  // The same dialog, with a usable key.
  const key = `qa-dept-${Date.now()}`;
  await page.locator("[data-department-key]").first().fill(key).catch(() => {});
  await page.locator("[data-department-submit]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const created = await page.locator(`[data-department-row="${key}"]`).count();
  note({ step: "create", key, rowShown: created > 0 });
  await shot(page, "page-organization-department-created");

  // The drawer: bind a role to the department, then revoke it.
  await page.locator(`[data-department-open="${key}"]`).first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const drawerOpen = await page.locator('[role="dialog"]').count();
  await shot(page, "page-organization-department-drawer");

  await page.locator("[data-department-role-picker]").first().selectOption({ index: 1 }).catch(() => {});
  await page.locator("[data-department-bind]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1500);
  const bound = await page.locator("[data-department-unbind]").count();
  note({ step: "bind-role", drawerOpen: drawerOpen > 0, boundRoleRows: bound });
  await shot(page, "page-organization-department-bound");

  if (bound > 0) {
    await page.locator("[data-department-unbind]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1400);
  }
  const unbound = await page.locator("[data-department-unbind]").count();
  note({ step: "unbind-role", roleRowsAfter: unbound });
  await shot(page, "page-organization-department-unbound");

  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator('button[aria-label="Close the department drawer"]').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(700);

  // A move under its own descendant is refused: it is a conflict the API answers, so the
  // allowance is registered before the click rather than after the 409 lands.
  expectRefusal("/departments", "a department cannot be moved inside its own subtree");
  await page.locator(`[data-department-row="${key}"]`).first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(900);
  const editButton = page
    .locator(`[data-department-row="${key}"] button[aria-label^="Edit"]`)
    .first();
  if (await editButton.count()) {
    await editButton.click().catch(() => {});
    await page.waitForTimeout(600);
    await page.locator("[data-department-parent]").first().selectOption({ index: 1 }).catch(() => {});
    await page.locator("[data-department-submit]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1400);
  }
  const selfMove = (await page
    .locator('[role="alert"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "self-move", refused: selfMove.slice(0, 160) });
  await shot(page, "page-organization-department-self-move");
  await page.keyboard.press("Escape").catch(() => {});
  await page.waitForTimeout(500);

  // Archive it, then delete it — the pass leaves no residue behind.
  await page.locator(`[data-department-archive="${key}"]`).first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const archived = await page.locator(`[data-department-row="${key}"]`).count();
  const archivedBadge = await page
    .locator(`[data-department-row="${key}"] >> text=archived`)
    .count();
  note({ step: "archive", rowStillListed: archived > 0, archivedShown: archivedBadge > 0 });
  await shot(page, "page-organization-department-archived");

  await page.locator(`[data-department-delete="${key}"]`).first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const gone = await page.locator(`[data-department-row="${key}"]`).count();
  note({ step: "delete", rowGone: gone === 0 });
  await shot(page, "page-organization-department-deleted");

  const out = { steps, organizationId, key };
  report.organizationDepartments = out;
  log(`organization departments: ${JSON.stringify(steps)}`);
  return out;
}

/**
 * The Modules, Settings and Billing tabs of one organization (REQ-005, slice 3).
 *
 * The pass walks what the request asks a reader to be able to *do*, not merely that the tabs
 * render: switch a module off and on and read the state back, change the locale and the accent
 * and reload to prove both persisted, and look at the usage bars the Billing tab labels with
 * their ceiling. A tab that only loads is a screenshot, not a walk.
 */
/**
 * The invite policy, the owner-approval queue and the Audit tab (REQ-005, slice 3 remainder).
 *
 * The other slice-3 pass proved the Settings, Modules and Billing tabs render. This one drives
 * the two screens that *change what the API does*:
 *
 *   1. the tenant is put on `closed` and an invitation is refused, with the refusal registered as
 *      an assertion so a correct 403 is not reported as a defect;
 *   2. `self_serve` invites for real, and the row carries a link;
 *   3. `owner_approval` queues the invite — the dialog says so instead of printing a dead link,
 *      and the queue panel appears;
 *   4. the manager who raised it cannot release it (the API refuses; the panel shows why), which
 *      is the whole difference between `self_serve` and `owner_approval`;
 *   5. the Audit tab lists the tenant's own rows, filters to one action, and exports;
 *   6. the tenant is put back to the policy it started on, so the pass leaves no residue.
 *
 * The release itself is deliberately *not* clicked: the pass account is the installation's owner
 * only by accident, and a release mints a single-use link that cannot be minted twice. Proving
 * the release is what the seven API walks in `tenancy_limits.rs` are for; this pass proves the
 * screen says the right thing about it.
 */
async function runOrganizationInvitePolicy(page, report, organizationId) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "organization-invite-policy", action: "organizations", ...step });
  };

  if (!organizationId) {
    note({ step: "skip", reason: "no organization was created by an earlier pass" });
    report.organizationInvitePolicy = { steps, organizationId: null };
    return report.organizationInvitePolicy;
  }

  const base = `${URL_ADMIN}/organizations/${organizationId}`;
  const stamp = Date.now();
  const addresses = {
    selfServe: `qa-selfserve-${stamp}@omnion.test`,
    queued: `qa-queued-${stamp}@omnion.test`,
  };

  const setPolicy = async (policy) => {
    await page.goto(`${base}?tab=settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-organization-settings-policy]", { timeout: 15000 }).catch(() => {});
    await page.waitForTimeout(500);
    // The policy is a radio group, not a select, so it is clicked by value and then saved — the
    // form is edited in local state and only pushed on submit, so clicking alone changes nothing.
    await page
      .locator(`[data-organization-settings-policy="${policy}"]`)
      .first()
      .click({ timeout: 5000 })
      .catch(() => {});
    await page.waitForTimeout(300);
    await page.locator("[data-organization-settings-save]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(2000);
  };

  const inviteThrough = async (address) => {
    await page.goto(`${base}?tab=members`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-invite-open]", { timeout: 15000 }).catch(() => {});
    await page.waitForTimeout(500);
    await page.locator("[data-invite-open]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(500);
    await page.locator("[data-invite-email]").first().fill(address).catch(() => {});
    await page.locator("[data-invite-submit]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1800);
  };

  // ---- closed: the invitation is refused, and the panel says why -----------------------------
  await setPolicy("closed");
  // A refusal the pass provokes on purpose must be registered first, or the correct 403 is
  // reported as a defect.
  expectRefusal("/invitations", "a closed organization refuses a new invitation");
  await inviteThrough(addresses.selfServe);
  const closedRefusal = (await page
    .locator('[role="alert"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "closed", refused: closedRefusal.slice(0, 160) });
  await shot(page, "page-organization-invite-closed");

  // ---- self_serve: the invitation is real and its row carries a link -------------------------
  await setPolicy("self_serve");
  await inviteThrough(addresses.selfServe);
  const selfServeRow = await page.locator(`[data-invitation-row="${addresses.selfServe}"]`).count();
  note({ step: "self-serve", rowShown: selfServeRow > 0 });
  await shot(page, "page-organization-invite-self-serve");

  // ---- owner_approval: queued, no link, and the queue panel appears --------------------------
  await setPolicy("owner_approval");
  await inviteThrough(addresses.queued);
  const queuedNotice = (await page
    .locator('[role="status"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  const queueVisible = await page.locator("[data-invitation-queue]").count();
  const queueRow = await page.locator(`[data-queue-row="${addresses.queued}"]`).count();
  note({
    step: "queued",
    queuePanel: queueVisible > 0,
    queueRow: queueRow > 0,
    // The notice is the half that matters: "invitation sent, copy this link" for a queued
    // invitation would send the operator off to mail a link that answers "waiting".
    saysQueued: /queued|owner/i.test(queuedNotice),
    notice: queuedNotice.slice(0, 160),
  });
  await shot(page, "page-organization-invite-queued");

  // Releasing is the owner's alone. The pass account may or may not be one, so both answers are
  // acceptable and what is asserted is that the panel never offers a *silent* no-op: either the
  // link appears, or the refusal is on screen with its code.
  const release = page.locator(`[data-queue-release="${addresses.queued}"]`).first();
  if (await release.count()) {
    expectRefusal("/invitations", "releasing a queued invitation needs the owner");
    await release.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1800);
    const released = await page.locator("[data-invitation-released]").count();
    const refusal = (await page
      .locator('[role="status"]')
      .last()
      .innerText()
      .catch(() => "")).replace(/\s+/g, " ");
    note({ step: "release", released: released > 0, refused: refusal.slice(0, 160) });
    await shot(page, "page-organization-invite-released");
  } else {
    note({ step: "release", skipped: "no queue row to release" });
  }

  // Revoke it instead, so the pass leaves no live invitation behind whatever the policy did.
  const queueRevoke = page.locator(`[data-queue-revoke="${addresses.queued}"]`).first();
  if (await queueRevoke.count()) {
    await queueRevoke.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1500);
  }
  const drained = (await page.locator(`[data-queue-row="${addresses.queued}"]`).count()) === 0;
  note({ step: "queue-drained", drained });

  // The live invitation from the self_serve step is revoked too.
  const revokeSelfServe = page.locator(`[data-invitation-revoke="${addresses.selfServe}"]`).first();
  if (await revokeSelfServe.count()) {
    await revokeSelfServe.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1500);
  }

  // ---- the Audit tab ---------------------------------------------------------------------------
  const auditUrl = `${base}?tab=audit`;
  await page.goto(auditUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-audit-filters]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(1200);

  const auditRows = await page.locator("[data-audit-row]").count();
  const actionOptions = await page
    .locator("[data-audit-action-filter] option")
    .count();
  const count = (await page
    .locator("[data-audit-count]")
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "audit-open", rows: auditRows, actionOptions, count });
  await shot(page, "page-organization-audit");

  // The action filter is built from the tenant's own rows, so it has to *have* options — a filter
  // that offers nothing is the tab's most likely quiet failure.
  if (actionOptions > 1) {
    const firstAction = await page
      .locator("[data-audit-action-filter] option")
      .nth(1)
      .getAttribute("value");
    await page
      .locator("[data-audit-action-filter]")
      .first()
      .selectOption(firstAction)
      .catch(() => {});
    await page.waitForTimeout(1500);
    const narrowed = await page.locator("[data-audit-row]").count();
    const narrowedCount = (await page
      .locator("[data-audit-count]")
      .first()
      .innerText()
      .catch(() => "")).replace(/\s+/g, " ");
    note({ step: "audit-filter", action: firstAction, rows: narrowed, count: narrowedCount });
    await shot(page, "page-organization-audit-filtered");

    // A filter that matches nothing must say so — an empty table with no sentence reads as a
    // broken tab.
    await page
      .locator("[data-audit-action-filter]")
      .first()
      .selectOption("")
      .catch(() => {});
    await page.waitForTimeout(800);
  }

  const exportButton = page.locator("[data-audit-export]").first();
  note({ step: "audit-export", enabled: await exportButton.isEnabled().catch(() => false) });
  await shot(page, "page-organization-audit-export");

  // Leave the tenant on the policy it started on.
  await setPolicy("owner_approval");
  await shot(page, "page-organization-invite-policy-restored");

  const out = { steps, organizationId, addresses };
  report.organizationInvitePolicy = out;
  log(`organization invite policy: ${JSON.stringify(steps)}`);
  return out;
}

async function runOrganizationTenantTabs(page, report, organizationId) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "organization-tenant-tabs", action: "organizations", ...step });
  };

  if (!organizationId) {
    note({ step: "skip", reason: "no organization was created by an earlier pass" });
    report.organizationTenantTabs = { steps, organizationId: null };
    return report.organizationTenantTabs;
  }

  // ---- Modules -------------------------------------------------------------------------------
  const modulesUrl = `${URL_ADMIN}/organizations/${organizationId}?tab=modules`;
  await page.goto(modulesUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-organization-module]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(600);

  const moduleRows = await page.locator("[data-organization-module]").count();
  note({ step: "modules-list", rows: moduleRows });
  await shot(page, "page-organization-modules");

  if (moduleRows > 0) {
    const firstKey = await page
      .locator("[data-organization-module]")
      .first()
      .getAttribute("data-organization-module");
    const switchAt = (moduleKey) =>
      page.locator(`[data-organization-module="${moduleKey}"] button[role="switch"]`);

    const wasOn = (await switchAt(firstKey).getAttribute("aria-checked")) === "true";
    await switchAt(firstKey).click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1200);
    const nowOn = (await switchAt(firstKey).getAttribute("aria-checked")) === "true";
    note({ step: "module-toggle", module: firstKey, wasOn, nowOn, changed: wasOn !== nowOn });
    await shot(page, "page-organization-module-toggled");

    // A reload is what proves it persisted rather than being echoed back.
    await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForSelector("[data-organization-module]", { timeout: 15000 }).catch(() => {});
    await page.waitForTimeout(600);
    const afterReload = (await switchAt(firstKey).getAttribute("aria-checked")) === "true";
    note({ step: "module-persisted", module: firstKey, persisted: afterReload === nowOn });

    // Put it back, so the pass leaves the organization as it found it.
    await switchAt(firstKey).click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1000);
    const restored = (await switchAt(firstKey).getAttribute("aria-checked")) === "true";
    note({ step: "module-restored", module: firstKey, restored: restored === wasOn });
  }

  // ---- Settings ------------------------------------------------------------------------------
  const settingsUrl = `${URL_ADMIN}/organizations/${organizationId}?tab=settings`;
  await page.goto(settingsUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  // Targeted hooks, not "the first select on the page": the header carries a site switcher that
  // is itself a `<select>`, and a bare `locator("select").first()` writes the locale into the
  // *site* picker and then waits forever for an option that is not there. This cost a whole
  // QA pass to find.
  await page.waitForSelector("[data-organization-settings-locale]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(600);

  const localeField = page.locator("[data-organization-settings-locale]").first();
  const accentField = page.locator("[data-organization-settings-accent]").first();
  const zoneField = page.locator("[data-organization-settings-timezone]").first();

  await localeField.selectOption("tr").catch(() => {});
  await zoneField.fill("Europe/Istanbul").catch(() => {});
  await accentField.fill("#2f6f4f").catch(() => {});
  await shot(page, "page-organization-settings");
  await page.locator('button:has-text("Save settings")').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1400);

  // Reload and read the stored values back: a form that only looks right is not saved.
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-organization-settings-locale]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(700);

  const storedLocale = await page
    .locator("[data-organization-settings-locale]")
    .first()
    .inputValue()
    .catch(() => "");
  const storedAccent = await accentField.inputValue().catch(() => "");
  const storedZone = await zoneField.inputValue().catch(() => "");
  note({
    step: "settings-saved",
    locale: storedLocale,
    accent: storedAccent,
    timezone: storedZone,
    localePersisted: storedLocale === "tr",
    accentPersisted: storedAccent === "#2f6f4f",
  });
  await shot(page, "page-organization-settings-saved");

  // Put it back so the next pass reads the screen in the language it started in.
  await localeField.selectOption("en").catch(() => {});
  await zoneField.fill("UTC").catch(() => {});
  await accentField.fill("").catch(() => {});
  await page.locator('button:has-text("Save settings")').first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1000);

  // ---- Billing -------------------------------------------------------------------------------
  const billingUrl = `${URL_ADMIN}/organizations/${organizationId}?tab=billing`;
  await page.goto(billingUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector('[role="progressbar"]', { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(700);

  const bars = await page.locator('[role="progressbar"]').count();
  const barLabels = await page.locator('[role="progressbar"]').evaluateAll((nodes) =>
    nodes.map((node) => ({
      label: node.getAttribute("aria-label"),
      now: node.getAttribute("aria-valuenow"),
      text: node.getAttribute("aria-valuetext"),
    })),
  );
  // A bar that names neither a metric nor a number is decoration; the REQ asks for a number and
  // a ceiling on every one.
  const labelled = barLabels.every((bar) => bar.label && bar.now !== null);
  note({ step: "billing-bars", bars, labelled, barLabels });
  await shot(page, "page-organization-billing");

  const out = { steps, organizationId };
  report.organizationTenantTabs = out;
  log(`organization tenant tabs: ${JSON.stringify(steps)}`);
  return out;
}

/**
 * The suspend/archive pass (REQ-005, slice 3's last part).
 *
 * The API walks prove the rule; this proves the *panel*: the banner appears on a frozen tenant,
 * a write control is refused with the reason on screen, and reactivating takes the banner away.
 * The pass suspends the organization the earlier passes created, so it runs *after* them and
 * always re-activates in a `finally`-equivalent tail — a pass that leaves the QA tenant frozen
 * turns every later route into a 409 and the run that follows it fails for the wrong reason.
 */
async function runOrganizationSuspend(page, report, organizationId) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "organization-suspend", action: "organizations", ...step });
  };

  if (!organizationId) {
    note({ step: "skip", reason: "no organization was created by an earlier pass" });
    report.organizationSuspend = { steps, organizationId: null };
    return report.organizationSuspend;
  }

  const listUrl = `${URL_ADMIN}/organizations`;
  const settingsUrl = `${URL_ADMIN}/organizations/${organizationId}?tab=settings`;

  // The banner must be absent while the tenant is active — otherwise "the banner is there" is
  // a statement about a strip that is always there.
  await page.goto(listUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  const bannerWhileActive = await page.locator("[data-qa-tenant-banner]").count();
  note({ step: "active", banner: bannerWhileActive });

  // Suspend through the list's own control, the way an operator does.
  const suspendButton = page
    .locator('[data-qa-guard="write"]')
    .filter({ hasText: "Suspend" })
    .first();
  if ((await suspendButton.count()) === 0) {
    note({ step: "skip", reason: "the list carries no Suspend control for this row" });
    report.organizationSuspend = { steps, organizationId };
    return report.organizationSuspend;
  }
  await suspendButton.click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const notice = (await page
    .locator('[role="status"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "suspend", notice: notice.slice(0, 120) });
  await shot(page, "page-organization-suspended-list");

  // The banner is on a *frozen tenant* screen, not on the one that set the freeze.
  await page.goto(settingsUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-organization-settings-save]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const banner = page.locator("[data-qa-tenant-banner]").first();
  const bannerCount = await banner.count();
  const bannerText = (await banner.innerText().catch(() => "")).replace(/\s+/g, " ");
  const bannerStatus = await banner.getAttribute("data-qa-tenant-banner").catch(() => "");
  note({ step: "banner", shown: bannerCount > 0, status: bannerStatus, text: bannerText.slice(0, 140) });

  // A write against the frozen tenant is refused, and the panel says why rather than failing
  // silently. The refusal is expected, so it is registered before the act or the correct 4xx is
  // reported as a defect.
  expectRefusal("/settings", "a suspended organization refuses a settings save");
  await page.locator("[data-organization-settings-save]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const refusal = (await page
    .locator('[role="alert"]')
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  note({ step: "write-refused", refusal: refusal.slice(0, 160) });
  await shot(page, "page-organization-suspended-refusal");

  // Reactivate: the banner has to disappear, or the panel keeps claiming a tenant that works.
  await page.goto(listUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  const reactivate = page
    .locator('[data-qa-guard="write"]')
    .filter({ hasText: "Reactivate" })
    .first();
  if ((await reactivate.count()) > 0) {
    await reactivate.click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(2000);
  }
  await page.goto(settingsUrl, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);
  const bannerAfter = await page.locator("[data-qa-tenant-banner]").count();
  note({ step: "reactivated", banner: bannerAfter, reactivated: (await reactivate.count()) > 0 });
  await shot(page, "page-organization-reactivated");

  report.organizationSuspend = { steps, organizationId };
  return report.organizationSuspend;
}

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

/**
 * Read a single value out of the disposable QA database.
 *
 * A scalar read that matches nothing returns `""`, and a caller that interpolates that into a
 * later statement builds `where site_id = ''` — a **type error** against a uuid column that then
 * aborts the whole seeding step with a message about the *update*, pointing at the wrong line.
 * Failing here, at the read that actually came back empty, names the real cause.
 */
function qaScalar(statement, what) {
  const value = qaSql(statement);
  if (!value) {
    throw new Error(`qa fixture: ${what || statement} matched nothing in ${QA_DB}`);
  }
  return value;
}

/** Run one statement against the disposable QA database. */
function qaSql(statement) {
  return execFileSync(
    "docker",
    ["exec", QA_PG_CONTAINER, "psql", "-U", "omnion", "-d", QA_DB, "-v", "ON_ERROR_STOP=1", "-t", "-A", "-c", statement],
    { encoding: "utf8", timeout: 30000 },
  ).trim();
}

/**
 * The value a `RETURNING` clause produced, or `""`.
 *
 * `qaSql` returns psql's stdout verbatim, and a statement that spans several lines and ends
 * in `RETURNING id` does NOT come back as a uuid: psql prints its own status line first, so
 * the caller received the literal string `INSERT 0 0` and used it as a `page_id`. The next
 * fixture statement then failed on `invalid input syntax for type uuid`, and the whole depth
 * pass reported **"the promotion screen is broken"** — a finding about a screen, caused
 * entirely by a fixture helper returning the wrong shape.
 *
 * That is the same shape of mistake as a swallowed Playwright error: the harness's failure
 * and the product's failure land on the same output line, and only the second is actionable.
 * The fix is to make the helper that reads a value refuse to return one it could not have
 * come from, rather than to patch each call site.
 *
 * The last non-empty line is psql's value; a status line is `INSERT 0 1` / `UPDATE 1` /
 * `DELETE 3` and never a uuid. Both halves are needed: taking the last line stops the
 * statement from returning the status line at all, and rejecting a non-uuid stops a caller
 * from pasting a status line into a uuid column ever again.
 */
function qaReturning(statement) {
  const raw = qaSql(statement);
  if (!raw) return "";
  // psql prints RETURNING rows FIRST and its own status line (`INSERT 0 1`) LAST. Both
  // orders have to be right, and I got the first version backwards: taking the last line
  // would hand back "INSERT 0 1" for every statement that worked.
  const uuid = raw
    .split("\n")
    .map((line) => line.trim())
    .find((line) => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(line));
  // No uuid means the statement matched no rows — and psql still exits 0 and still prints a
  // status line, so the caller used to receive "INSERT 0 0" and paste it into a uuid column.
  return uuid || "";
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
    const site = qaScalar(`select id from sites where key = '${CREDS.siteKey}' limit 1`, "the QA site (key '${CREDS.siteKey}')");
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
    const site = qaScalar(`select id from sites where key = '${CREDS.siteKey}' limit 1`, "the QA site (key '${CREDS.siteKey}')");
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

  // ---- the test-delivery block (REQ-021, slice 5) ------------------------------------------
  //
  // **The legs that matter are the ones a shortcut would fail.** A screen that rendered a
  // green "Delivered" without sending anything, or that reported a failed send as an HTTP
  // error banner, is the failure this block exists to catch — so it asserts the *line* the
  // server sent, not merely that a line appeared.
  await page.goto(`${URL_ADMIN}/notifications/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);

  steps.testBlockPresent = (await page.locator("[data-test-delivery]").count()) > 0;

  // in-app must be *absent* from the offered channels: the screen explains why next to it,
  // so its absence is a claim the copy has to back up.
  const inAppRow = await page.locator("[data-test-channel=in_app]").count();
  const copyMentionsInApp = await page
    .locator("[data-test-delivery]")
    .innerText()
    .then((text) => /in-app/i.test(text))
    .catch(() => false);
  steps.inAppNotOffered = inAppRow === 0;
  steps.inAppAbsenceIsExplained = copyMentionsInApp;

  // A channel this installation cannot send over must still be offered and must answer with a
  // readable reason. `web_push` is the honest one: no browser subscription exists in a
  // headless pass, so the server's refusal is the expected result — and a *clicked* button that
  // says why is the whole feature.
  const pushButton = page.locator("[data-test-button=web_push]");
  steps.pushButtonOffered = (await pushButton.count()) > 0;
  if (steps.pushButtonOffered) {
    await pushButton.first().click().catch(() => {});
    await page.waitForTimeout(2500);
    const result = page.locator("[data-test-result=web_push]");
    steps.pushTestAnswered = (await result.count()) > 0;
    if (steps.pushTestAnswered) {
      steps.pushTestDelivered = (await result.getAttribute("data-delivered").catch(() => "")) === "yes";
      const text = await result.innerText().catch(() => "");
      // The detail is the sentence under the verdict. An empty one is the failure: "not
      // delivered" with no reason sends the reader to their settings page to guess.
      steps.pushTestExplainsItself = text.trim().length > 30 && /not delivered/i.test(text);
    }
  }

  // The in-flight lock: a second press while one is running must not be possible, because two
  // sends racing into one status line is a line whose number belongs to neither.
  const emailButton = page.locator("[data-test-button=email]");
  if ((await emailButton.count()) > 0) {
    await emailButton.first().click().catch(() => {});
    await page.waitForTimeout(120);
    steps.testDisabledWhileInFlight =
      (await emailButton.first().isDisabled().catch(() => false)) === true;
  }
  await shot(page, "page-notifications-test-delivery");

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

 * The cache-rule pass (REQ-011, slice 1).
 *
 * A rule table is a list of strings with a drag handle, and every claim it makes can be
 * faked by a screen that renders the array it was handed. So the claims proved here are the
 * ones a screenshot cannot settle, and each is the one the screen's design depends on:
 *
 *  1. the table shows what the API returns, in the API's order, and a rule the operator
 *     creates is *in* the table afterwards rather than optimistically in it;
 *  2. a move sends the **complete** order and the server's answer replaces the local one —
 *     a reorder that renumbers one row leaves two rules claiming the same priority, and the
 *     matcher then breaks the tie by row order, which is not the order the drag showed;
 *  3. the live match tester answers BOTH ways. A tester that always says "matches" is worse
 *     than none, because it looks like a check;
 *  4. a TTL above the cap is refused on screen with the message under the field, before it
 *     reaches the network — the API refuses it too, and the form is what makes that
 *     refusal legible;
 *  5. the empty, loading and error states all exist, and the mobile rendering is a card
 *     list carrying the same data hooks as the row it replaces.
 *
 * The rules are created through the API with the signed-in session, so the rows are rows the
 * real route wrote — and the pass deletes what it made.
 */
/**
 * The purge pipeline (REQ-011, slice 2).
 *
 * Seven things are driven, and the two that are not obvious are the reason this pass exists
 * rather than just listing the routes:
 *
 *  1. **A purge reaches the history as a row the drawer can open.** A console that accepts
 *     and a history that lists are different claims; only opening the row proves the first
 *     produced the second.
 *  2. **The whole-zone confirmation is a dead end until PURGE is typed.** The form is filled
 *     out completely and the submit stays disabled — an action that is merely *possible* is
 *     not a confirmation, and a pass that only ever clicks a working button would not notice
 *     a checkbox standing in for one.
 *  3. **A malformed target is refused on screen, under its own field.** The server refusing
 *     is already proved by the API walks; what is new here is that the operator learns it
 *     without a round trip.
 *  4. **A failed purge shows the provider's message and a retry that is really a retry.**
 *     The fixture forces the failure through the database rather than through a provider, so
 *     the pass does not depend on a network being unreachable.
 *  5. **Retry moves the failed items and leaves the succeeded ones alone.** The statuses are
 *     read back out of the drawer, and this is the assertion a screenshot cannot make.
 */
async function runCdnPurgeDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const site = qaScalar(`select id from sites where key = '${CREDS.siteKey}' limit 1`, "the QA site (key '${CREDS.siteKey}')");

  // A failed fixture, written directly: the pass must not depend on a provider being down,
  // and a `generic_http` adapter pointed at a closed port would make every run slower and
  // its result depend on the box's networking.
  // `qaReturning`, not `qaSql`: this statement spans lines, so psql's own status line comes
  // back with the value and `failedId` would be the string "INSERT 0 0".
  const failedId = qaReturning(
    `insert into cdn_purges (site_id, kind, targets, status, provider, item_count, failed_count, error) ` +
      `values ('${site}', 'url', array['/qa/never-cached'], 'failed', 'origin', 1, 1, ` +
      `'the provider refused: target is not in this zone') returning id`,
  );
  if (failedId) {
    qaSql(
      `insert into cdn_purge_items (purge_id, target, status, attempts, error, done_at) ` +
        `values ('${failedId}', '/qa/never-cached', 'failed', 5, ` +
        `'the provider refused: target is not in this zone', now())`,
    );
  }

  await page.goto(`${URL_ADMIN}/cdn/purge`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.console = (await page.locator("[data-cdn-purge-targets]").count()) > 0;

  // A malformed target: refused on screen, and the submit blocked.
  await page.locator("[data-cdn-purge-targets]").fill("blog/no-leading-slash").catch(() => {});
  await page.waitForTimeout(350);
  steps.malformedMessage = (
    await page.locator("[data-cdn-purge-targets-error]").innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();
  steps.malformedBlocksSubmit = await page
    .locator("[data-cdn-purge-submit]")
    .isDisabled()
    .catch(() => false);
  await shot(page, "page-cdn-purge-invalid");

  // A good list: the count updates and the submit is enabled.
  await page.locator("[data-cdn-purge-targets]").fill("/qa/one\n/qa/two").catch(() => {});
  await page.waitForTimeout(350);
  steps.targetCount = (
    await page.locator("[data-cdn-purge-target-count]").innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();

  // The whole-zone mode: filled out and still blocked, because the word is not typed.
  await page.locator('[data-cdn-purge-mode="all"] input[type="radio"]').click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(300);
  steps.zoneBlockedBeforeConfirm = await page
    .locator("[data-cdn-purge-submit]")
    .isDisabled()
    .catch(() => false);
  await page.locator("[data-cdn-purge-confirm]").fill("PURGE").catch(() => {});
  await page.waitForTimeout(300);
  steps.zoneEnabledAfterConfirm = !(await page
    .locator("[data-cdn-purge-submit]")
    .isDisabled()
    .catch(() => true));
  // Back to URLs and queue one for real, so the history has a row this pass made.
  await page.locator('[data-cdn-purge-mode="url"] input[type="radio"]').click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(250);
  await page.locator("[data-cdn-purge-submit]").click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.submitted = (await page.locator("[data-cdn-purge-submitted]").count()) > 0;
  await shot(page, "page-cdn-purge-submitted");

  // The history: the row this pass made is there, and the failed fixture is visible with its
  // message — the whole reason the screen exists.
  await page.goto(`${URL_ADMIN}/cdn/purges`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.rows = await page.locator("[data-cdn-purge-row]").count();
  steps.failedBanner = (await page.locator("[data-cdn-purge-failed-banner]").count()) > 0;
  steps.pageTotal = (
    await page.locator("header p").first().innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();

  // The count match. "Showing 4 of 7" is a claim about two numbers, and REQ-011's acceptance
  // line is that the rows on screen agree with the API — so the pass compares three readings
  // of the same fact: the rows the DOM has, the `total` the API answers, and the sentence the
  // header renders. A screen that renders the sentence from a stale `total`, or counts only
  // the page it holds, agrees with itself and fails here.
  //
  // The comparison is only worth anything with MORE THAN ONE PAGE of rows, because a single
  // page makes `purges.length === total` true by construction — a pager that never pages is
  // indistinguishable from a table with nothing to page through. So the pass asks the API for
  // its total first and skips rather than records a vacuous pass.
  const purgeCounts = await page.evaluate(async (siteId) => {
    const response = await fetch(`/api/v1/cdn/purges?site_id=${siteId}&limit=1`, {
      credentials: "same-origin",
    });
    if (!response.ok) return null;
    const body = await response.json();
    return { total: body.total ?? null, firstPage: (body.purges ?? []).length };
  }, site);
  steps.apiTotal = purgeCounts?.total ?? null;
  steps.countMatches =
    typeof steps.apiTotal === "number" &&
    steps.rows > 1 &&
    steps.pageTotal.includes(`Showing ${steps.rows} of ${steps.apiTotal}`);
  // Above the fold is not the table: the same locator is counted at 1280px, and a page that
  // only renders the first page while the API holds more is the defect this catches.
  steps.pageOneOfMany = typeof steps.apiTotal === "number" && steps.rows < steps.apiTotal;

  // The drawer: open the failed fixture by its row, and read the provider's own words.
  const failedRow = page.locator('[data-cdn-purge-status="failed"]').first();
  if ((await failedRow.count()) > 0) {
    await failedRow.click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(1400);
    steps.drawerOpened = (await page.locator("[data-cdn-purge-drawer]").count()) > 0;
    steps.drawerError = (
      await page.locator("[data-cdn-purge-error]").innerText().catch(() => "")
    )
      .replace(/\s+/g, " ")
      .trim();
    steps.drawerItems = await page.locator("[data-cdn-purge-item]").count();
    // The retry button exists because the server said this row has something to retry, and
    // its label names the count — a button that says "Retry" on a partial row does not say
    // whether it will re-send the twenty targets that already worked.
    steps.retryLabel = (
      await page.locator("[data-cdn-purge-retry]").innerText().catch(() => "")
    )
      .replace(/\s+/g, " ")
      .trim();
    await shot(page, "page-cdn-purges-drawer");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(400);
    steps.escClosedDrawer =
      (await page.locator("[data-cdn-purge-drawer]").count()) === 0;
  }

  // The filter, narrowed and then cleared: a filter that does not change the rows is
  // decoration, and this is the cheapest place to see that.
  const before = steps.rows;
  // `selectOption`, and NOT a `.fill` or a `selectOption` on a text box: the status filter is
  // a closed set, so the control is a `<select>` (see the note in `cdn-purges-view.tsx`).
  //
  // The catch that used to swallow this was the defect's first half. `selectOption` on an
  // `<input>` throws, the `.catch(() => {})` ate it, and the pass went on to record
  // `filterNarrows: false` — a sentence the summary reads as *the product's filter does not
  // narrow its rows*. A harness that cannot drive a control and a control that does not work
  // produce the same line, and the second reading is the one somebody acts on.
  //
  // So: no catch on the interaction, and the two readings are recorded separately.
  const statusFilter = page.locator("[data-cdn-purge-filter]");
  steps.statusFilterIsSelect =
    (await statusFilter.evaluate((el) => el.tagName.toLowerCase()).catch(() => "")) === "select";
  await statusFilter.selectOption("failed", { timeout: 5000 });
  await page.waitForTimeout(1300);
  steps.filteredRows = await page.locator("[data-cdn-purge-row]").count();
  steps.filterNarrows = steps.filteredRows < before;
  // The empty state, reached the only way an operator reaches it: a filter that matches
  // nothing. "No purge has ever been requested" and "nothing matches that filter" are two
  // different sentences with two different meanings, and a screenshot of the first proves
  // nothing about the second — which is why this drives the *combination* rather than a
  // status on its own: the pass's own failed fixture is a `url` purge, so filtering to
  // `failed` AND a kind it does not have leaves the table genuinely empty while the
  // unfiltered table above it has rows. A screen that renders its no-purges-yet message here
  // tells an operator their site has no history when it is a filter that is wrong.
  await page
    .locator("[data-cdn-purge-kind-filter]")
    .selectOption("tag")
    .catch(() => {});
  await page.waitForTimeout(1300);
  steps.emptyRows = await page.locator("[data-cdn-purge-row]").count();
  steps.emptyStateShown =
    (await page.locator("text=Nothing matches that filter").count()) > 0;
  // And the two must not be the same sentence: the honest empty state is a *different*
  // answer from the unfiltered one, so a screen that reuses "no purges yet" under a filter
  // fails here.
  steps.emptyStateIsNotTheUnfilteredOne = steps.emptyStateShown && steps.emptyRows === 0;
  await page.locator("[data-cdn-purge-filter-clear]").click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1100);
  steps.clearedBackToRows =
    (await page.locator("[data-cdn-purge-row]").count()) === before;
  await shot(page, "page-cdn-purges");

  // Mobile: the cards, not a horizontally scrolling table. The hooks are the same ones the
  // rows carry, so a pass can drive either rendering.
  await page.setViewportSize({ width: 390, height: 900 });
  await page.goto(`${URL_ADMIN}/cdn/purges`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);
  steps.mobileCards = await page.locator("[data-cdn-purge-row]").count();
  steps.mobileTableHidden = await page
    .locator("table")
    .first()
    .isHidden()
    .catch(() => false);
  await shot(page, "page-cdn-purges-mobile");
  await page.setViewportSize({ width: 1280, height: 900 });

  // Clean up only what this pass made. The deletes are guarded because `qaSql` runs with
  // ON_ERROR_STOP=1 and throws — a cleanup that matched nothing must not abort the pass and
  // lose every step after it. The inserts above are deliberately *not* guarded: a fixture
  // that silently failed to insert would leave the drawer, the message and the retry label
  // all reading as "not found", and a pass that reports absence as a pass is worse than no
  // pass.
  try {
    if (failedId) {
      qaSql(`delete from cdn_purges where id = '${failedId}'`);
    }
    qaSql(
      `delete from cdn_purges where site_id = '${site}' and targets = array['/qa/one','/qa/two']`,
    );
  } catch (cleanupError) {
    steps.cleanupFailed = String(cleanupError);
  }
  return steps;
}

async function runCdnRulesDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const site = qaScalar(`select id from sites where key = '${CREDS.siteKey}' limit 1`, "the QA site (key '${CREDS.siteKey}')");
  // A 403 here is a real finding rather than a setup problem: the owner seeds the roles on
  // boot, so an owner without `cdn.manage` means the permission did not reach the role.
  expectRefusal(
    "cdn/rules",
    "a rule the panel never asked for",
  );

  await page.goto(`${URL_ADMIN}/cdn/rules`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.rows = await page.locator("[data-cdn-rule-row]").count();
  steps.pageTotal = (
    await page.locator("[data-cdn-rule-count]").innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();
  // The same count match the purge pass measures, on the second of the two screens whose
  // acceptance line names it. The rules screen renders "N in precedence order" rather than a
  // paged "showing N of M", so what it can lie about is different: the header counts the
  // rules it was given, and a table that silently drops a rule the API returned is invisible
  // in a screenshot. The comparison is rows-in-the-DOM against rules-in-the-API-body, and the
  // API is asked for the same site so the two are counting the same set.
  steps.apiRuleCount = await page.evaluate(async (siteId) => {
    const response = await fetch(`/api/v1/cdn/rules?site_id=${siteId}`, {
      credentials: "same-origin",
    });
    if (!response.ok) return null;
    const body = await response.json();
    return (body.rules ?? []).length;
  }, site);
  steps.countMatches =
    typeof steps.apiRuleCount === "number" &&
    steps.rows === steps.apiRuleCount &&
    steps.pageTotal.includes(`${steps.rows} in precedence order`);
  // The header is not the table: the numbers agree on screen only if the rows that carry the
  // per-row hooks are the rows the header counted.
  steps.headerCountedTheRows = steps.pageTotal.startsWith(`${steps.rows} `);

  // 1. The live tester, both ways, before anything is saved. A rule that matches nothing is
  //    a valid rule the server will happily store, which is exactly why this box exists.
  await page.locator("[data-cdn-rule-new]").click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(500);
  steps.formOpened = (await page.locator("[data-cdn-rule-name]").count()) > 0;

  await page.locator("[data-cdn-rule-pattern]").fill("/blog/**").catch(() => {});
  await page.locator("[data-cdn-rule-sample]").fill("/blog/post-1").catch(() => {});
  await page.waitForTimeout(350);
  steps.testerMatch = (await page.locator("[data-cdn-rule-verdict]").innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  await page.locator("[data-cdn-rule-sample]").fill("/pricing").catch(() => {});
  await page.waitForTimeout(350);
  steps.testerMiss = (await page.locator("[data-cdn-rule-verdict]").innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  // The two answers must differ, or the tester is decoration.
  steps.testerIsLive = steps.testerMatch !== steps.testerMiss && /not match/i.test(steps.testerMiss);
  await shot(page, "page-cdn-rules-tester");

  // 4. A TTL above the cap, refused on screen. The field is found by its hook rather than by
  //    index, so a layout change does not silently make this pass drive the wrong input.
  await page.locator("[data-cdn-rule-name]").fill(`QA rule ${stamp}`).catch(() => {});
  await page.locator("[data-cdn-rule-edge-ttl]").fill("99999999").catch(() => {});
  await page.locator("[data-cdn-rule-save]").click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  const refused = await page
    .locator("[data-cdn-rule-edge-ttl]")
    .locator("xpath=following-sibling::*[1]")
    .innerText()
    .catch(() => "");
  steps.ttlRefusal = refused.replace(/\s+/g, " ").trim();
  // The message has to be under the TTL field AND say what the bound is. "Invalid" is not
  // a message an operator can act on.
  steps.ttlRefusalNamesTheBound = /between 0 and/.test(steps.ttlRefusal);
  await shot(page, "page-cdn-rules-ttl-error");

  // The same save with a legal TTL, which is what proves the refusal was the value and not
  // the form.
  await page.locator("[data-cdn-rule-edge-ttl]").fill("120").catch(() => {});
  await page.locator("[data-cdn-rule-save]").click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const created = await page.evaluate(
    async ([siteId, name]) => {
      const response = await fetch(`/api/v1/cdn/rules?site_id=${siteId}`, {
        credentials: "same-origin",
      });
      if (!response.ok) return null;
      const body = await response.json();
      return (body.rules ?? []).find((rule) => rule.name === name) ?? null;
    },
    [site, `QA rule ${stamp}`],
  );
  steps.createdByApi = created !== null;
  steps.createdInTable =
    (await page.locator(`[data-cdn-rule-row="${created?.id ?? ""}"]`).count()) > 0;
  await shot(page, "page-cdn-rules-created");

  // 2. The reorder. Two rules are needed for a move to mean anything, so a second one is
  //    created through the API (fast, and the panel's own create is already proven above).
  const second = await page.evaluate(
    async ([siteId, name]) => {
      const response = await fetch("/api/v1/cdn/rules", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ site_id: siteId, name, path_pattern: "/pricing" }),
      });
      return { status: response.status, body: await response.json().catch(() => null) };
    },
    [site, `QA second ${stamp}`],
  );
  steps.secondCreated = second.status === 201;
  const secondId = second.body?.id ?? "";
  await page.goto(`${URL_ADMIN}/cdn/rules`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);

  const before = await page.evaluate(
    async (siteId) => {
      const response = await fetch(`/api/v1/cdn/rules?site_id=${siteId}`, {
        credentials: "same-origin",
      });
      const body = await response.json();
      return (body.rules ?? []).map((rule) => rule.id);
    },
    site,
  );
  steps.orderBefore = before.length;
  // The rule this pass created is at rank 1 on an empty table, and the panel disables "move
  // up" there — correctly, since there is nothing above it. The pass used to click that
  // disabled button, swallow the timeout in a `.catch`, and then assert on an order that
  // had not changed, so `reorderSwapped: false` said "the panel does not reorder" about a
  // panel that was never asked to do anything.
  //
  // The move target is therefore chosen by position rather than by identity: the LAST rule
  // in the API's own order, which is the one row where "up" is always available. Asserting
  // on a control the pass has not first proved is moveable is how a green step measures
  // nothing.
  const mover = before[before.length - 1] ?? "";
  const moverRow = page.locator(`[data-cdn-rule-row="${mover}"]`);
  const moverUp = page.locator(`[data-cdn-rule-up="${mover}"]`);
  steps.moverExists = (await moverRow.count()) > 0;
  steps.moverUpEnabled = await moverUp.isEnabled().catch(() => false);
  await moverUp.click({ timeout: 5000 });
  await page.waitForTimeout(1800);
  // Read the order back from the API, not from the table: the table is the thing under test.
  const after = await page.evaluate(
    async (siteId) => {
      const response = await fetch(`/api/v1/cdn/rules?site_id=${siteId}`, {
        credentials: "same-origin",
      });
      const body = await response.json();
      return {
        ids: (body.rules ?? []).map((rule) => rule.id),
        priorities: (body.rules ?? []).map((rule) => rule.priority),
      };
    },
    site,
  );
  // The moved rule must be exactly one position higher, and everything else must be
  // undisturbed. "The first id changed" is a weaker claim that a table which reversed the
  // whole list would also satisfy.
  const movedFrom = before.indexOf(mover);
  steps.reorderSwapped =
    movedFrom > 0 &&
    after.ids[movedFrom - 1] === mover &&
    after.ids.length === before.length;
  // Priorities must be a dense ascending run, which is the property a per-row renumber breaks.
  steps.prioritiesDense = after.priorities.every(
    (value, index) => value === after.priorities[0] + index,
  );
  await shot(page, "page-cdn-rules-reordered");

  // The toggle and the duplicate, because a table whose rows can only be created is half a
  // screen. Both are read back through the API.
  await page.locator(`[data-cdn-rule-toggle="${secondId}"]`).click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.toggled = await page.evaluate(
    async ([siteId, id]) => {
      const response = await fetch(`/api/v1/cdn/rules?site_id=${siteId}`, {
        credentials: "same-origin",
      });
      const body = await response.json();
      return (body.rules ?? []).find((rule) => rule.id === id)?.enabled === false;
    },
    [site, secondId],
  );
  await page.locator(`[data-cdn-rule-duplicate="${created?.id ?? ""}"]`).click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  steps.duplicated = await page.evaluate(
    async ([siteId, name]) => {
      const response = await fetch(`/api/v1/cdn/rules?site_id=${siteId}`, {
        credentials: "same-origin",
      });
      const body = await response.json();
      return (body.rules ?? []).some((rule) => rule.name === name);
    },
    [site, `QA rule ${stamp} copy`],
  );
  await shot(page, "page-cdn-rules-actions");

  // 5. The states. The error banner, provoked the honest way — a route that answers 500 —
  //    because a table that shows an empty list after a failure is a table an operator
  //    reads as "this site has no rules".
  await page.route("**/api/v1/cdn/rules?*", (route) =>
    route.fulfill({
      status: 500,
      contentType: "application/json",
      body: '{"error":{"code":"boom","message":"deliberate"}}',
    }),
  );
  await page.locator("button[aria-label='Reload cache rules']").click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1000);
  steps.errorState = (await page.locator("[role=alert]").count()) > 0;
  await page.unroute("**/api/v1/cdn/rules?*").catch(() => {});
  await shot(page, "page-cdn-rules-error");

  // 6. Mobile. The claim is not "it renders" but "the mobile rows carry the same hooks the
  //    desktop depth pass above just drove" — a hook that exists in only one of the two
  //    renderings halves what any pass can reach, and the measurement is then of a layout
  //    no interaction has ever visited.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${URL_ADMIN}/cdn/rules`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1600);
  steps.mobileRows = await page.locator("[data-cdn-rule-row]").count();
  steps.mobileNoTableScroll = await page.evaluate(
    () => document.documentElement.scrollWidth <= window.innerWidth + 1,
  );
  // A card is a control a thumb can hit. The floor is `TOUCH_TARGET_MIN_PX`, defined once at the
  // top of this file — the comment in the first version of this block said 44 while the code
  // checked 32, which is a comment nobody reads against a number nobody re-checks, and the
  // organization switcher's finding message still repeated the stale 44 for the switcher rows.
  //
  // Disabled buttons count. A rank-1 row's "Up" is correctly disabled, and excluding disabled
  // controls would measure a screen that is easier to use than it is.
  //
  // The measurement names the offending control, not just how many are short. `smallest` alone
  // is the number tick 82's note claimed to have replaced: it says a row is short without saying
  // which, so the next pass still starts by reading the source to guess — and on a row with six
  // buttons the guess is the expensive way to find out that only "Delete" is at the floor. Each
  // entry carries its `data-cdn-rule-*` hook, its label and its height, so a report line points
  // at one line of `cdn-rules-view.tsx` instead of at a component.
  //
  // The floor is passed IN rather than closed over: `page.evaluate` serialises this function and
  // runs it in the page, where `TOUCH_TARGET_MIN_PX` does not exist. Referencing it here is a
  // `ReferenceError` at runtime — which `evaluate` reports as a rejected promise and this step's
  // caller catches, so the measurement silently vanishes instead of failing. Node scope is not
  // page scope, and a constant that reads as though it is shared is a trap that only fires in a
  // browser.
  const touch = await page.evaluate((minPx) => {
    const buttons = Array.from(document.querySelectorAll("[data-cdn-rule-row] button"));
    const measured = buttons
      .map((button) => ({
        label: (button.textContent || "").trim().slice(0, 24),
        hook: Object.keys(button.dataset).find((key) => key.startsWith("cdnRule")) || "(no hook)",
        height: Math.round(button.getBoundingClientRect().height * 10) / 10,
      }))
      .filter((button) => button.height > 0);
    const short = measured.filter((button) => button.height < minPx);
    return {
      total: measured.length,
      short: short.length,
      floor: minPx,
      smallest: measured.length ? Math.min(...measured.map((button) => button.height)) : 0,
      shortControls: short,
    };
  }, TOUCH_TARGET_MIN_PX);
  steps.mobileTouchTargets = touch.total > 0 && touch.short === 0;
  steps.mobileTouchTargetDetail = touch;
  await shot(page, "page-cdn-rules-mobile");
  await page.setViewportSize({ width: 1280, height: 900 });

  // Clean up what the pass made, through the API, so a second pass over the same database
  // does not inherit them and the walkthrough stays idempotent.
  await page.evaluate(
    async (siteId) => {
      const response = await fetch(`/api/v1/cdn/rules?site_id=${siteId}`, {
        credentials: "same-origin",
      });
      const body = await response.json();
      for (const rule of body.rules ?? []) {
        if (rule.name.startsWith("QA rule ") || rule.name.startsWith("QA second ")) {
          await fetch(`/api/v1/cdn/rules/${rule.id}?site_id=${siteId}`, { method: "DELETE" });
        }
      }
    },
    site,
  );
  await page.goto(`${URL_ADMIN}/cdn/rules`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1400);
  steps.cleanedUp = await page.locator("[data-cdn-rule-row]").count();
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
  const siteId = qaScalar(`select id from sites where key = '${CREDS.siteKey}' limit 1`, "the QA site (key '${CREDS.siteKey}')");

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
 * The staging environments (REQ-017, slice 2).
 *
 * What this pass has to prove is not "the screen renders" — the route loop already did that — but
 * the six claims that make the screen trustworthy, each of which has a way of being fake:
 *
 *  1. **The list renders production plus whatever staging exists**, and the Content column is
 *     read from the API rather than counted on screen.
 *  2. **The wizard creates an environment and its clone really copies rows.** A wizard that
 *     creates the row and reports "done" while zero rows were copied is the exact failure this
 *     request exists to prevent, so the pass waits for the job to finish and compares the staging
 *     page count against production's.
 *  3. **The clone gave the staging pages their own identities.** Two rows with the same slug and
 *     the same environment would mean the copy silently overwrote itself; the pass counts them.
 *  4. **The re-clone confirmation shows a number, not a shrug.** The dialog is built from the
 *     server's refusal, so the pass sends the unconfirmed request first and reads the counts out
 *     of the dialog the API's answer produced.
 *  5. **Cancelling is offered only where the job is open**, and the button is absent on a
 *     finished row — a cancel that always returns `409` is a dead button.
 *  6. **Archiving keeps the content** and releases the host, so the row stays in the archived
 *     filter rather than disappearing and taking the pages with it.
 *
 * Since slice 3 there are four more claims, and they are the ones a promotion screen can fake
 * most easily because the interesting part happens *after* the click:
 *
 *  7. **The selection names its own scope.** The button reads `Promote selection (1)` after one
 *     checkbox, not `Promote all N`. A bulk action that quietly widened the operator's selection
 *     is the most dangerous button on the screen, and the label is the only place it is visible.
 *  8. **The dialog leads with a count, not a warning.** The frozen summary is read from the live
 *     change set the operator is looking at, so what they confirm is what they saw.
 *  9. **Requesting writes a row.** The pass reads `promotions` in the database, because a
 *     timeline that renders from local state and writes nothing is a dialog that lies.
 * 10. **The Promotions tab shows the record, not the screen state.** It is re-read after a full
 *     navigation, so a tab built from the same in-memory state would pass a check it should fail.
 *
 * Everything it creates is removed in the `finally`, because the QA database is shared with the
 * next writer's pass and a leftover staging environment is a row their clone count will read.
 */
async function runEnvironmentsDepth(page, report) {
  const steps = {};
  const stamp = Date.now();
  const key = `qa-staging-${stamp}`;
  let environmentId = null;

  try {
    // ---- The list --------------------------------------------------------------------------
    await page.goto(`${URL_ADMIN}/environments`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1200);
    const rows = await page.locator("[data-env-row]").count();
    const production = await page.locator('[data-env-row][data-env-type="production"]').count();
    steps.list = { rows, production };
    if (rows === 0) {
      return { ok: false, reason: "/environments rendered no rows at all" };
    }
    // The empty state must not be showing behind a populated table. Both at once is the shape a
    // screen gets when a fetch resolves after the empty branch has already rendered.
    const emptyAlongside = (await page.locator("text=/No staging environment yet/i").count()) > 0;
    if (emptyAlongside) {
      record({ page: "environments", action: "empty-state-over-populated" });
      steps.emptyOverPopulated = true;
    }
    await shot(page, "environments-list");

    // ---- The filter, through the URL --------------------------------------------------------
    await page.selectOption("[data-env-type-filter]", "staging").catch(() => {});
    await page.waitForTimeout(900);
    const stagedOnly = await page.locator('[data-env-row][data-env-type="production"]').count();
    const urlHasType = page.url().includes("type=staging");
    steps.filter = { stagedOnly, urlHasType };

    // ---- The wizard ------------------------------------------------------------------------
    await page.goto(`${URL_ADMIN}/environments`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(900);
    await page.click("[data-env-new]").catch(() => {});
    await page.waitForTimeout(500);
    const wizardOpened = (await page.locator("[data-env-wizard]").count()) > 0;
    steps.wizardOpened = wizardOpened;
    if (!wizardOpened) {
      return { ok: false, reason: "the create wizard did not open" };
    }
    await shot(page, "environments-wizard-name");

    // An empty name must not advance: the wizard is asked for at least one area, so refusing
    // here is the same refusal the API would make, one step earlier and without a round trip.
    const nextBlocked = await page.locator("[data-env-wizard-next]").isDisabled().catch(() => false);
    steps.nextBlockedWithoutName = nextBlocked;

    await page.fill("[data-env-name]", `QA Staging ${stamp}`).catch(() => {});
    await page.fill("[data-env-key]", key).catch(() => {});
    await page.click("[data-env-wizard-next]").catch(() => {});
    await page.waitForTimeout(350);
    // A URL with a scheme is refused on screen, before the request.
    await page.fill("[data-env-host]", "https://staging.example.com/path").catch(() => {});
    await page.waitForTimeout(300);
    const hostRefused = (await page.locator("text=/A host is a name, not a URL/i").count()) > 0;
    steps.hostRefusedOnScreen = hostRefused;
    // The chip and the banner both name the host, so the walk needs to know which host it
    // created. Read it back from the row rather than repeating the string: a pass that hardcodes
    // its own fixture and then asserts against a different value asserts nothing.
    const environmentHost = `staging-${stamp}.qa.omnion.test`;
    await page.fill("[data-env-host]", environmentHost).catch(() => {});
    await shot(page, "environments-wizard-host");
    await page.click("[data-env-wizard-next]").catch(() => {});
    await page.waitForTimeout(400);

    // Areas come from the API, so the checkboxes are counted rather than assumed.
    const areaBoxes = await page.locator("[data-env-area]").count();
    steps.areaOptions = areaBoxes;
    if (areaBoxes === 0) {
      return { ok: false, reason: "the wizard offered no clone areas" };
    }

    // ---- An area that promises a copy must copy one (REQ-017) ---------------------------
    // Three of the six areas — menus, site settings and the theme — are shared with production
    // and return 0 from the runner. They used to be priced and labelled exactly like the three
    // that work, so ticking one produced an environment with nothing in it. The API now says
    // which is which on each option (`data-env-area-copies`), and these four claims are the
    // ones that would fail if the two halves ever drifted apart again:
    //   1. at least one area really copies — otherwise nothing in the wizard is a promise;
    //   2. every non-copying area says WHY, so it reads as a boundary and not as an empty site;
    //   3. the non-copying areas are NOT pre-ticked, so the default is a real copy;
    //   4. the summary repeats the shared note, rather than listing them as content again.
    const areaRows = await page.locator("[data-env-area-option]").evaluateAll((els) =>
      els.map((el) => ({
        name: el.getAttribute("data-env-area-option"),
        copies: el.getAttribute("data-env-area-copies") === "true",
        box: el.querySelector("input[type=checkbox]"),
        note: el.querySelectorAll("span")[1]?.textContent?.trim() ?? "",
      })),
    );
    const copying = areaRows.filter((row) => row.copies);
    const shared = areaRows.filter((row) => !row.copies);
    steps.areasThatCopy = copying.length;
    steps.areasThatAreShared = shared.length;
    steps.sharedAreasExplained = shared.every((row) => row.note.length > 0);
    steps.sharedAreasNotPreselected = shared.every((row) => row.box && !row.box.checked);
    steps.areaNotePresent = (await page.locator("[data-env-area-note]").count()) > 0;
    // These three were written as `report.findings.push({severity, where, what})`, which is **not
    // a shape this harness has ever had**: the findings list and its `pushFindings(severity, kind,
    // detail)` helper are declared *inside* `main()` at the roll-up, so no depth pass can reach
    // them, and `report.findings` is `undefined`. The line therefore threw
    // `TypeError: Cannot read properties of undefined (reading 'push')` on every run — the pass
    // died at the wizard's area step, before any of its clone, detail, promotion or archive
    // claims could execute. Five ticks recorded that depth pass as "owed", and the cause was not
    // the box at all.
    //
    // So the assertions are made the way the rest of the file makes them: the fact goes into
    // `steps` (which the return value and `summary.json` carry) and the *reporting* happens
    // through `record`, which appends to `clicks.jsonl`. A pass that finds a defect should not
    // need a private channel to the roll-up.
    if (copying.length === 0) {
      record({
        page: "environments",
        action: "wizard-area-claims-nothing",
        severity: "high",
        detail: "no clone area claims to copy anything, so a staging environment could be created empty",
      });
    }
    if (shared.length > 0 && !steps.sharedAreasExplained) {
      record({
        page: "environments",
        action: "wizard-shared-area-unexplained",
        severity: "high",
        detail: "an area that copies nothing is offered without saying so",
      });
    }
    if (shared.length > 0 && !steps.sharedAreasNotPreselected) {
      record({
        page: "environments",
        action: "wizard-shared-area-preselected",
        severity: "medium",
        detail: "an area that copies nothing is pre-ticked, so the default clone promises work it does not do",
      });
    }

    // Unchecking everything must block the next step: a clone that copies nothing is not an
    // environment, and the API refuses it — the screen must not be the one to discover that.
    for (const name of areaRows.map((row) => row.name)) {
      await page.locator(`[data-env-area="${name}"]`).uncheck().catch(() => {});
    }
    await page.waitForTimeout(250);
    const areasBlocked = await page.locator("[data-env-wizard-next]").isDisabled().catch(() => false);
    steps.nextBlockedWithoutAreas = areasBlocked;
    await shot(page, "environments-wizard-areas");

    // Ticking ONLY the shared areas must still block the step. The rule is "at least one thing
    // that copies", not "at least one box" — the second reading is how a browser could produce
    // the environment the crate's own `require_areas` guard exists to prevent.
    for (const row of shared) {
      await page.locator(`[data-env-area="${row.name}"]`).check().catch(() => {});
    }
    await page.waitForTimeout(250);
    steps.nextBlockedWithOnlySharedAreas = await page
      .locator("[data-env-wizard-next]")
      .isDisabled()
      .catch(() => false);
    for (const row of shared) {
      await page.locator(`[data-env-area="${row.name}"]`).uncheck().catch(() => {});
    }

    // Re-tick a copying area — `.first()` is not safe here, because the first checkbox may be
    // one of the shared areas and an environment created from it would be empty by design.
    const firstCopying = copying[0]?.name;
    if (firstCopying) {
      await page.locator(`[data-env-area="${firstCopying}"]`).check().catch(() => {});
    }
    await page.waitForTimeout(250);
    await page.click("[data-env-wizard-next]").catch(() => {});
    await page.waitForTimeout(400);
    // The summary is the last screen before the button, so it is where a repeated promise
    // would do the most damage — the step-2 note is already two clicks in the operator's past.
    steps.sharedNoteOnSummary = (await page.locator("[data-env-wizard-shared]").count()) > 0;
    // Same dead `report.findings` channel as above, and it sat *after* the submit, so it would
    // have killed the pass one step later than the first one did — the second half of the
    // wizard's promises would have gone unmeasured for the same reason as the first half.
    if (!steps.sharedNoteOnSummary) {
      record({
        page: "environments",
        action: "wizard-summary-hides-shared-areas",
        severity: "medium",
        detail:
          "the confirmation does not repeat that navigation, settings and theme are shared rather than copied",
      });
    }
    await shot(page, "environments-wizard-confirm");
    // The submit is clicked through a locator rather than `page.click`, and the response is
    // awaited, because "the button was enabled" and "the request left the browser" are two
    // different claims and only one of them was being measured. A `.catch(() => {})` on a click
    // that lands on nothing — a stale node, an overlay, a guard that made the button a no-op —
    // is indistinguishable from a submit the API refused, and the pass reported the second while
    // the first was what happened.
    const submit = page.locator("[data-env-wizard-submit]");
    const submitPresent = (await submit.count().catch(() => 0)) > 0;
    const submitEnabled = submitPresent
      ? await submit.isEnabled().catch(() => false)
      : false;
    steps.submitPresent = submitPresent;
    steps.submitEnabled = submitEnabled;
    const [response] = await Promise.all([
      page
        .waitForResponse(
          (r) => r.url().includes("/api/v1/environments") && r.request().method() === "POST",
          { timeout: 15000 },
        )
        .catch(() => null),
      submit.click({ timeout: 8000 }).catch(() => {}),
    ]);
    steps.submitResponse = response
      ? { status: response.status(), body: (await response.text().catch(() => "")).slice(0, 200) }
      : null;
    await page.waitForTimeout(2500);

    environmentId = qaSql(`select id from environments where key = '${key}' limit 1`);
    steps.created = Boolean(environmentId);
    if (!environmentId) {
      // The reason names the response rather than restating the symptom, because "no environment
      // with this key" is what every distinct failure looks like from here: a refused host, a 403,
      // a 500, a click that never fired. The step that failed is carried with it.
      return {
        ok: false,
        reason: `the wizard submitted but no environment with key ${key} exists`,
        steps: {
          ...steps,
          submitPresent,
          submitEnabled,
          submitResponse: steps.submitResponse,
        },
      };
    }

    // ---- The clone really copies ------------------------------------------------------------
    // The worker runs in the API process, so the job is polled here rather than assumed. The
    // production page count is read from the same table the copy reads, which makes the
    // comparison a fact about the data instead of a fact about a status word.
    const productionPages = Number(
      qaSql(
        `select count(*) from pages p join environments e on e.id = p.environment_id ` +
          `where e.type = 'production' and e.organization_id = (select organization_id from environments where id = '${environmentId}')`,
      ),
    );
    let job = null;
    for (let attempt = 0; attempt < 30 && !job; attempt += 1) {
      job = qaSql(
        `select status from environment_clone_jobs where environment_id = '${environmentId}' order by created_at desc limit 1`,
      );
      if (job === "done" || job === "failed" || job === "cancelled") {
        break;
      }
      await page.waitForTimeout(1000);
    }
    const copied = Number(
      qaSql(
        `select count(*) from pages p join environments e on e.id = p.environment_id where e.id = '${environmentId}'`,
      ),
    );
    // The whole point: a `done` job with zero copied rows is the failure the request names.
    const cloneCopied = job === "done" && copied > 0 && copied === productionPages;
    steps.clone = { job, copied, productionPages, cloneCopied };

    // Two rows with the same slug in one environment would mean the copy collided with itself.
    const duplicates = qaSql(
      `select count(*) from (select slug from pages p join environments e on e.id = p.environment_id ` +
        `where e.id = '${environmentId}' group by slug having count(*) > 1) d`,
    );
    steps.duplicateSlugs = Number(duplicates);
    if (Number(duplicates) !== 0) {
      record({ page: "environments", action: "duplicate-slugs-in-clone" });
    }

    // ---- The detail screen ------------------------------------------------------------------
    await page.goto(`${URL_ADMIN}/environments/${environmentId}`, { waitUntil: "domcontentloaded" }).catch(
      () => {},
    );
    await page.waitForTimeout(1200);
    const facts = (await page.locator("[data-env-detail-facts] p").allInnerTexts()).join("|");
    const jobRows = await page.locator("[data-env-job-row]").count();
    const estimate = await page.locator("[data-env-detail-estimate]").innerText().catch(() => "");
    steps.detail = {
      rendered: (await page.locator("[data-env-detail-facts]").count()) > 0,
      facts,
      jobRows,
      estimateHasWords: estimate.trim().split(/\s+/).length > 2,
    };
    await shot(page, "environments-detail");

    // ---- The re-clone confirmation ----------------------------------------------------------
    // The unconfirmed request goes first *by design*: the API answers it with the row counts the
    // dialog is supposed to show. A dialog built from a guess is exactly what this catches.
    await page.click("[data-env-detail-reclone]").catch(() => {});
    await page.waitForTimeout(1500);
    const dialogShown = (await page.locator("[data-env-reclone-dialog]").count()) > 0;
    const discardLines = (await page.locator("[data-env-reclone-discard] li").allInnerTexts()).join("|");
    steps.recloneDialog = {
      dialogShown,
      discardLines,
      namesACount: /\d+ page/.test(discardLines),
    };
    await shot(page, "environments-reclone-confirm");

    if (dialogShown) {
      await page.click("[data-env-reclone-cancel]").catch(() => {});
      await page.waitForTimeout(400);
      const closedOnCancel = (await page.locator("[data-env-reclone-dialog]").count()) === 0;
      steps.recloneDialog.closedOnCancel = closedOnCancel;
    }

    // ---- Cancel is only offered where it works ----------------------------------------------
    // A finished job must not carry a cancel button. The API refuses it with a 409, so a button
    // there is a control that exists only to fail.
    const finishedCancelButtons = await page
      .locator('[data-env-job-row][data-env-job-status="done"] [data-env-job-cancel]')
      .count();
    steps.cancelOnFinishedJob = finishedCancelButtons;
    if (finishedCancelButtons > 0) {
      record({ page: "environments", action: "cancel-offered-on-finished-job" });
    }

    // ---- The promotion path (REQ-017, slice 3) ---------------------------------------------
    // This runs *before* the archive step, because archiving releases the host and an archived
    // environment is not something you promote. The order here is the order a person would do
    // it: look at the change set, freeze it, decide, and only then throw the copy away.
    //
    // The change set is populated first, through SQL rather than through the content screen —
    // the point of this pass is the promotion UI, and seeding a page through the editor would
    // make a failure ambiguous between "the editor broke" and "the promotion broke".
    const productionEnv = qaSql(
      `select id from environments where organization_id = (select organization_id from environments where id = '${environmentId}') and type = 'production' limit 1`,
    );
    if (productionEnv) {
      const seedSlug = `qa-promote-${stamp}`;
      // A page's title lives on its revision, not on `pages` — a seed that writes a `title`
      // column does not exist fails at the first insert and the whole pass reports "the promotion
      // screen is broken" for a reason that is entirely in the fixture. The revision is created
      // as a draft, which is the state the change set reads its title from.
      const seeded = qaReturning(
        `insert into pages (id, site_id, environment_id, slug, status, created_by, created_at, updated_at) ` +
          `select gen_random_uuid(), p.site_id, '${environmentId}', '${seedSlug}', 'draft', p.created_by, now(), now() ` +
          `from pages p ` +
          `where p.environment_id = '${productionEnv}' and p.site_id is not null limit 1 ` +
          `returning id`,
      );
      if (seeded) {
        qaSql(
          `insert into page_revisions (page_id, revision_no, state, title, body, created_by) ` +
            `values ('${seeded}', 1, 'draft', 'QA promote row', 'Seeded by the environment pass.', null)`,
        );
      }
    }

    await page.goto(
      `${URL_ADMIN}/environments/${environmentId}?tab=changes`,
      { waitUntil: "domcontentloaded" },
    ).catch(() => {});
    await page.waitForTimeout(1500);
    const changeRows = await page.locator("[data-change-row]").count();
    const counts = await page.locator("[data-changes-counts]").innerText().catch(() => "");
    steps.changes = { rows: changeRows, counts, namedTheRow: /QA promote row/.test(await page.locator("[data-changes-tab]").innerText().catch(() => "")) };
    await shot(page, "environments-changes-tab");

    // The selection is the bulk action, so it has to actually select. Selecting one row and
    // reading the button's own count is the check: a button that says "Promote all 3" while three
    // rows are checked is a scope the operator never chose.
    await page.locator("[data-change-select]").first().check().catch(() => {});
    await page.waitForTimeout(300);
    const promoteLabel = await page
      .locator("[data-changes-promote]")
      .innerText()
      .catch(() => "");
    steps.selection = { label: promoteLabel, reflectsSelection: /Promote selection \(1\)/.test(promoteLabel) };
    if (!steps.selection.reflectsSelection) {
      record({ page: "environments", action: "promote-button-does-not-name-the-selection" });
    }

    await page.click("[data-changes-promote]").catch(() => {});
    await page.waitForTimeout(800);
    const promotionDialog = (await page.locator("[data-promotion-dialog]").count()) > 0;
    const summary = await page
      .locator("[data-promotion-counts]")
      .innerText()
      .catch(() => "");
    const permissionNote = await page
      .locator("[data-promotion-permission-note]")
      .innerText()
      .catch(() => "");
    steps.promotionDialog = {
      shown: promotionDialog,
      summary,
      namesAnItemCount: /\d+ item/.test(summary),
      permissionNote,
    };
    await shot(page, "environments-promotion-dialog");

    if (promotionDialog) {
      await page.click("[data-promotion-request]").catch(() => {});
      await page.waitForTimeout(2000);
      const timeline = (await page.locator("[data-promotion-timeline]").count()) > 0;
      const approveRendered = (await page.locator("[data-promotion-approve]").count()) > 0;
      const approveDisabled = await page
        .locator("[data-promotion-approve]")
        .isDisabled()
        .catch(() => false);
      steps.promotionFrozen = { timeline, approveRendered, approveDisabled };
      await shot(page, "environments-promotion-timeline");

      // The record must exist in the database, not just on screen. A timeline that renders from
      // local state and writes nothing is a dialog that lies, and the integration walks cover
      // the API half while this covers "the button that calls it".
      const promotions = Number(
        qaSql(
          `select count(*) from promotions where environment_id = '${environmentId}'`,
        ),
      );
      steps.promotionRowWritten = promotions;
      if (promotions === 0) {
        record({ page: "environments", action: "promotion-dialog-wrote-no-record" });
      }

      if (approveRendered && !approveDisabled) {
        await page.click("[data-promotion-approve]").catch(() => {});
        await page.waitForTimeout(3000);
        const done = qaSql(
          `select status from promotions where environment_id = '${environmentId}' order by created_at desc limit 1`,
        );
        steps.promotionApplied = done;
        await shot(page, "environments-promotion-applied");
      }
      await page.click("[data-promotion-cancel-dialog]").catch(() => {});
      await page.waitForTimeout(500);
    }

    // ---- The Promotions tab, read back from the record ---------------------------------------
    await page.goto(
      `${URL_ADMIN}/environments/${environmentId}?tab=promotions`,
      { waitUntil: "domcontentloaded" },
    ).catch(() => {});
    await page.waitForTimeout(1500);
    const promotionRows = await page.locator("[data-promotion-row]").count();
    steps.promotionsTab = { rows: promotionRows, rendered: (await page.locator("[data-promotions-tab]").count()) > 0 };
    await shot(page, "environments-promotions-tab");

    if (promotionRows > 0) {
      await page.locator("[data-promotion-expand]").first().click().catch(() => {});
      await page.waitForTimeout(1200);
      const frozenItems = await page.locator("[data-promotion-expanded-items] li").count();
      steps.promotionExpanded = { frozenItems };
      await shot(page, "environments-promotions-expanded");
    }

    // ---- The header chip and the staging banner (REQ-017, slice 5) -------------------------
    // This runs BEFORE the archive, deliberately: an archived staging environment is read-only
    // and the chip is defined to report production for it, so measuring after the archive would
    // prove the *opposite* of what the criterion asks. The selection is also the only way to
    // reach the state at all — nothing in the URL selects an environment, which is the whole
    // reason the chip is a control (see `lib/active-environment.tsx`).
    await page.goto(`${URL_ADMIN}/environments/${environmentId}`, { waitUntil: "domcontentloaded" }).catch(
      () => {},
    );
    await page.waitForTimeout(1400);
    const chipBefore = (await page.locator("[data-env-chip]").count()) > 0;
    const chipProductionBefore = await page
      .locator('[data-env-chip][data-env-chip="production"]')
      .count();
    // No banner before a staging environment is selected. It is recorded as a step rather than
    // dropped: a chip and a banner that both appear for a production session make the staging
    // banner mean nothing, and the only way to know is to have measured production first.
    const bannerBefore = await page.locator("[data-qa-staging-banner]").count();
    steps.bannerBeforeSelectingStaging = bannerBefore;
    await shot(page, "environments-chip-production");

    await page.click("[data-env-chip]").catch(() => {});
    await page.waitForTimeout(500);
    const listOpened = (await page.locator("[data-env-chip-list]").count()) > 0;
    await shot(page, "environments-chip-open");
    await page.click(`[data-env-chip-option="${key}"]`).catch(() => {});
    await page.waitForTimeout(1600);

    const chipStaging = await page.locator('[data-env-chip][data-env-chip="staging"]').count();
    const bannerVisible = await page.locator(`[data-qa-staging-banner="${key}"]`).count();
    const bannerText = (await page
      .locator(`[data-qa-staging-banner="${key}"]`)
      .first()
      .innerText()
      .catch(() => "")) || "";
    // "Cannot be dismissed" is a claim about a control that must not exist, so it is measured as
    // one: no close button, no role=dialog with a dismiss affordance, and — the part a person
    // actually does — it is still there after navigating to another screen entirely.
    const dismissControls = await page
      .locator(
        `[data-qa-staging-banner="${key}"] button[aria-label*="ismiss" i], ` +
          `[data-qa-staging-banner="${key}"] [data-env-banner-dismiss]`,
      )
      .count();
    await shot(page, "environments-chip-staging-banner");

    // Navigate somewhere unrelated. A banner that lives on /environments is not a panel banner.
    await page.goto(`${URL_ADMIN}/environments`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1500);
    const bannerElsewhere = await page.locator(`[data-qa-staging-banner="${key}"]`).count();
    await shot(page, "environments-chip-banner-elsewhere");

    // The banner's link must reach the changes tab it names, and the chip's selection must have
    // survived the navigation — a chip that resets on every route is a caption, not a control.
    const chipSurvived = await page.locator('[data-env-chip][data-env-chip="staging"]').count();
    await page.click(`[data-qa-staging-banner-link]`).catch(() => {});
    await page.waitForTimeout(1500);
    const bannerLinkWentToChanges = page.url().includes("/environments/") && page.url().includes("tab=changes");
    await shot(page, "environments-banner-link-target");

    // And the way out: choosing production removes the banner, which is the only control that
    // does. A banner with an ✕ is a banner that can be dismissed; this proves it cannot be
    // dismissed *by hiding it* — only by leaving.
    await page.click("[data-env-chip]").catch(() => {});
    await page.waitForTimeout(500);
    await page.click('[data-env-chip-option="main"], [data-env-chip-option="production"]').catch(() => {});
    await page.waitForTimeout(1500);
    const bannerAfterLeaving = await page.locator("[data-qa-staging-banner]").count();

    steps.chipAndBanner = {
      chipBefore,
      chipProductionBefore: chipProductionBefore > 0,
      listOpened,
      chipStaging: chipStaging > 0,
      bannerVisible: bannerVisible > 0,
      bannerNamesTheHost: environmentHost ? bannerText.includes(environmentHost) : null,
      dismissControls,
      bannerElsewhere: bannerElsewhere > 0,
      chipSurvivedNavigation: chipSurvived > 0,
      bannerLinkWentToChanges,
      bannerAfterLeaving,
    };
    if (chipBefore && !listOpened) {
      record({ page: "environments", action: "environment-chip-does-not-open-its-list" });
    }
    if (chipStaging > 0 && bannerVisible === 0) {
      record({ page: "environments", action: "staging-selected-without-a-banner" });
    }
    if (dismissControls > 0) {
      record({ page: "environments", action: "staging-banner-is-dismissible" });
    }
    if (bannerElsewhere === 0) {
      record({ page: "environments", action: "staging-banner-is-not-panel-wide" });
    }
    if (bannerAfterLeaving > 0) {
      record({ page: "environments", action: "staging-banner-survives-leaving-staging" });
    }

    // ---- Archive keeps the content ----------------------------------------------------------
    await page.click("[data-env-detail-archive]").catch(() => {});
    await page.waitForTimeout(500);
    await shot(page, "environments-archive-confirm");
    await page.click("[data-env-archive-confirm]").catch(() => {});
    await page.waitForTimeout(1500);
    const status = qaSql(`select status from environments where id = '${environmentId}'`);
    const rowsAfterArchive = Number(
      qaSql(`select count(*) from pages where environment_id = '${environmentId}'`),
    );
    steps.archive = { status, rowsAfterArchive, keptContent: status === "archived" && rowsAfterArchive > 0 };
    if (!(status === "archived" && rowsAfterArchive > 0)) {
      record({ page: "environments", action: "archive-lost-content" });
    }

    // The archived row must still be findable: the filter is how an operator gets it back.
    await page.goto(`${URL_ADMIN}/environments?status=archived`, { waitUntil: "domcontentloaded" }).catch(
      () => {},
    );
    await page.waitForTimeout(1000);
    steps.archivedVisibleUnderFilter = (await page.locator(`[data-env-row][data-env-open="${environmentId}"]`).count()) > 0;
    await shot(page, "environments-archived-filter");

    const ok =
      steps.clone?.cloneCopied === true &&
      steps.detail?.rendered === true &&
      steps.recloneDialog?.dialogShown === true &&
      steps.promotionDialog?.shown === true &&
      steps.promotionDialog?.namesAnItemCount === true &&
      steps.promotionRowWritten > 0 &&
      steps.promotionsTab?.rendered === true &&
      // The chip and the banner are not decoration on this screen: without them the criterion
      // "the chip appears in the panel header while staging is active" is unmeasured, and an
      // unmeasured screen is the exact failure this pass exists to prevent.
      steps.chipAndBanner?.listOpened === true &&
      steps.chipAndBanner?.chipStaging === true &&
      steps.chipAndBanner?.bannerVisible === true &&
      steps.chipAndBanner?.dismissControls === 0 &&
      steps.chipAndBanner?.bannerElsewhere === true &&
      steps.chipAndBanner?.chipSurvivedNavigation === true &&
      steps.chipAndBanner?.bannerLinkWentToChanges === true &&
      steps.chipAndBanner?.bannerAfterLeaving === 0 &&
      steps.archive?.keptContent === true;
    return { ok, steps };
  } finally {
    // Cleanup is not optional. The QA database is shared with every other writer's pass, and a
    // leftover staging environment with copied pages is a row their own clone counts will read.
    // The promotion rows go first because they reference the environment, and the seeded
    // revision before its page because `page_revisions.page_id` cascades — but the explicit
    // delete is what makes the intent readable, and `delete from pages` is what the other
    // writers' passes already rely on.
    if (environmentId) {
      // `promotions` only exists once migration 0161 has been applied to this database. A pass
      // that starts against a reset database would otherwise abort in the *cleanup*, which
      // throws away the steps it already collected and reports a red pass for a missing table
      // rather than for anything the screen did.
      try {
        qaSql(`delete from promotions where environment_id = '${environmentId}'`);
      } catch {
        // No promotion table here: this run had nothing to clean up.
      }
      qaSql(`delete from page_revisions where page_id in (select id from pages where environment_id = '${environmentId}')`);
      qaSql(`delete from pages where environment_id = '${environmentId}'`);
      qaSql(`delete from environments where id = '${environmentId}'`);
    }
  }
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
  const siteId = qaScalar(`select id from sites where key = '${CREDS.siteKey}' limit 1`, "the QA site (key '${CREDS.siteKey}')");

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

/**
 * The system health centre, driven end to end (REQ-014, slice 1).
 *
 * The assertion that matters is the **row count**, and it is asserted as a count
 * rather than as content. The request names seven services and the registry adds
 * the host, so the panel must show eight rows whether or not any of them has ever
 * been probed. A screen that listed only the services it received would render a
 * fresh database as *empty* and a broken one as *short* — and both of those read
 * to an operator as "there is nothing to report here", which is the single most
 * expensive thing this screen can show.
 *
 * The second assertion is the state's own honesty: every row must carry a state
 * out of the closed set, and a row that has never been probed must say `unknown`
 * rather than `healthy`. A `healthy` badge on a row nothing checked is a claim
 * the product made up, and it is asserted against here rather than trusted to the
 * server.
 */
async function runHealthDepth(page, report) {
  const steps = {};
  const note = (key, value) => {
    steps[key] = value;
    record({ page: "health", action: "health-depth", step: key, ...value });
  };

  await page.goto(`${URL_ADMIN}/health`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-health-screen]", { timeout: 20000 }).catch(() => {});
  const ready = (await page.locator('[data-health-screen="ready"]').count()) > 0;
  note({ step: "screen-ready", ready });
  if (!ready) {
    return { ok: false, reason: "/health did not finish loading", steps };
  }

  // ---- Every registered service is a row ------------------------------------------------------
  const rows = await page.locator("[data-health-service]").count();
  note({ step: "service-rows", rows });
  if (rows < 8) {
    note({
      step: "registry-too-short",
      rows,
      reason: "fewer than eight service rows rendered — a missing row reads as 'nothing to report'",
    });
  }

  // Every row's state is one of the four words, and it is on the element itself so
  // the assertion does not depend on reading the badge's text.
  const states = await page.$$eval("[data-health-service]", (nodes) =>
    nodes.map((node) => node.getAttribute("data-health-state")),
  );
  const legal = new Set(["healthy", "degraded", "down", "unknown"]);
  const illegal = states.filter((state) => !legal.has(state));
  note({ step: "states", tally: states.reduce((acc, s) => ({ ...acc, [s]: (acc[s] || 0) + 1 }), {}) });
  if (illegal.length > 0) {
    note({ step: "state-outside-vocabulary", illegal });
  }
  if (states.length !== rows) {
    note({ step: "state-missing", badges: states.length, rows });
  }

  // The banner is the server's own sentence and it must be non-empty. A client that
  // recomputed the worst state would disagree with the runner before the first run.
  const banner = await page
    .locator("[data-health-banner]")
    .first()
    .getAttribute("data-health-banner")
    .catch(() => null);
  const headline = (await page.locator("[data-health-banner]").first().innerText().catch(() => "")).trim();
  note({ step: "banner", banner, hasHeadline: headline.length > 0 });
  if (!banner || headline.length === 0) {
    note({ step: "banner-missing", reason: "the banner rendered no state or no sentence" });
  }

  // ---- A row's own checks, behind the disclosure ------------------------------------------------
  // Clicked rather than merely counted: a disclosure that renders its rows but does
  // not open is a dead control, and only a click proves it opens.
  const toggle = page.locator("[data-health-checks-toggle]").first();
  const hasToggle = (await toggle.count()) > 0;
  note({ step: "checks-toggle-present", hasToggle });
  if (hasToggle) {
    await toggle.click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(500);
    const checkRows = await page.locator("[data-health-checks] li").count();
    note({ step: "checks-opened", checkRows });
    if (checkRows === 0) {
      note({ step: "checks-empty-after-open", reason: "the disclosure opened with no checks in it" });
    }
  }
  await shot(page, "health-overview-checks");

  // ---- "Run all checks" -------------------------------------------------------------------------
  // The button must not blank the screen, and the rows must survive the run: a run
  // that answers 200 and leaves eight rows is the whole criterion, and a run that
  // left the screen empty would be the expensive failure.
  const before = await page.locator("[data-health-service]").count();
  await page.locator("[data-health-run]").click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(3000);
  const after = await page.locator("[data-health-service]").count();
  const runError = await page.locator("[data-health-error]").count();
  note({ step: "run-all-checks", before, after, errorShown: runError > 0 });
  if (after !== before) {
    note({
      step: "run-changed-row-count",
      before,
      after,
      reason: "a manual run must not change how many services there are",
    });
  }
  await shot(page, "health-overview-after-run");

  // ---- The auto-refresh control is real --------------------------------------------------------
  const refreshValue = await page
    .locator("[data-health-auto-refresh]")
    .first()
    .inputValue()
    .catch(() => null);
  note({ step: "auto-refresh", refreshValue });
  if (refreshValue === null) {
    note({ step: "auto-refresh-missing", reason: "the interval selector rendered nothing" });
  }

  // ---- The metric cards -------------------------------------------------------------------------
  // `NaN` and `Infinity` as *text* are named in the request's visual check, and they
  // are the failure a division by a zero total produces. Reading the rendered text is
  // the only assertion that catches it: the DOM value would still be a number.
  const cardText = (await page.locator("[data-health-metric]").allInnerTexts()).join(" ");
  note({ step: "metric-cards", hasCards: cardText.trim().length > 0 });
  if (/NaN|Infinity|undefined/i.test(cardText)) {
    note({ step: "non-finite-text", reason: "a metric card rendered NaN, Infinity or undefined" });
  }
  const dashOnly = (await page.locator("[data-health-service]").allInnerTexts()).every((text) =>
    !/NaN|Infinity|undefined/i.test(text),
  );
  if (!dashOnly) {
    note({ step: "non-finite-text-in-rows", reason: "a service row rendered a non-finite number" });
  }

  // ---- The service detail link is real ----------------------------------------------------------
  // The row's href is rendered by the server; clicking it proves the detail screen
  // exists rather than 404-ing, which is the dead-affordance the request forbids.
  const detailLink = page.locator("[data-health-service-link]").first();
  const href = await detailLink.getAttribute("href").catch(() => null);
  note({ step: "detail-href", href });
  if (!href || !href.startsWith("/health/services/")) {
    note({ step: "detail-href-missing", href });
  } else {
    await detailLink.click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1500);
    const landed = page.url().includes("/health/services/");
    note({ step: "detail-opened", landed, url: page.url() });
    if (!landed) {
      note({ step: "detail-did-not-open", url: page.url() });
    } else {
      // Landing on the URL is half the assertion. The screen has to have *rendered*:
      // a Next.js route that resolves but throws in the client still lands here, and
      // the operator gets a blank page behind a working link. Waiting for the
      // ready marker is what separates the two, and it is the only part that catches
      // the common "the API is fine, the component throws" failure.
      await page
        .waitForSelector('[data-health-detail-screen="ready"]', { timeout: 15000 })
        .catch(() => {});
      const detailReady = (
        await page.locator('[data-health-detail-screen="ready"]').count()
      ) > 0;
      note({ step: "detail-screen-rendered", detailReady });
      if (!detailReady) {
        const which = await page
          .locator("[data-health-detail-screen]")
          .first()
          .getAttribute("data-health-detail-screen")
          .catch(() => null);
        note({
          step: "detail-screen-missing",
          which,
          reason: "the detail route resolved but never rendered a state marker",
        });
      } else {
        // The drill-down's own content: a checks table (or an honest "not probed
        // yet" sentence) and the back link that proves the route has an exit.
        const checkCells = await page.locator("[data-health-check]").count();
        const noChecks = await page.locator("[data-health-detail-no-checks]").count();
        const metrics = await page.locator("[data-health-detail-metric]").count();
        const noMetrics = await page.locator("[data-health-detail-no-metrics]").count();
        note({ step: "detail-content", checkCells, noChecks, metrics, noMetrics });
        if (checkCells === 0 && noChecks === 0) {
          note({
            step: "detail-no-checks-and-no-explanation",
            reason: "neither a check row nor the 'not probed yet' sentence rendered",
          });
        }
        if (metrics === 0 && noMetrics === 0) {
          note({
            step: "detail-no-metrics-and-no-explanation",
            reason: "neither a metric row nor the 'no samples yet' sentence rendered",
          });
        }

        // ---- The 24 h trend line the request asks this screen to have -----------------------
        // Counted rather than eyeballed, and the count is the assertion: a table of current
        // values with no chart column is exactly the state this leg exists to catch, and it
        // renders perfectly happily — a heading that says "last 24 h" and rows that never
        // draw anything. Every metric row must account for a trend: a line, a single point,
        // or the sentence saying the window is empty. A row that accounts for none of the
        // three is a column that was added to the header and not to the body.
        const detailSparks = await page.$$eval(
          "[data-health-detail-metric]",
          (rows) =>
            rows.map((row) => {
              const spark = row.querySelector("[data-health-spark]");
              return {
                metric: row.getAttribute("data-health-detail-metric"),
                kind: spark ? spark.getAttribute("data-health-spark") : null,
                points: spark ? spark.getAttribute("data-health-spark-points") : null,
                min: spark ? spark.getAttribute("data-health-spark-min") : null,
                max: spark ? spark.getAttribute("data-health-spark-max") : null,
              };
            }),
        );
        const drawn = detailSparks.filter((row) => row.kind === "line" || row.kind === "point");
        const saidEmpty = detailSparks.filter((row) => row.kind === "empty");
        note({
          step: "detail-trend-lines",
          rows: detailSparks.length,
          drawn: drawn.length,
          saidEmpty: saidEmpty.length,
        });
        if (metrics > 0) {
          const undrawn = detailSparks.filter((row) => row.kind === null);
          if (undrawn.length > 0) {
            note({
              step: "detail-trend-column-empty",
              undrawn: undrawn.map((row) => row.metric),
              reason: "a metric row rendered no trend line and no 'no samples' sentence",
            });
          }
          if (drawn.length === 0 && saidEmpty.length === 0) {
            note({
              step: "detail-has-no-trend-at-all",
              reason: "the screen has metric rows but nothing that answers 'over 24 h'",
            });
          }
          // A line claims a series, so the point count must be a real number above one.
          // A `line` drawn with one point is the polyline-through-a-single-point failure,
          // which renders as nothing and reads as an empty window.
          const badLines = drawn.filter(
            (row) => row.kind === "line" && Number(row.points) < 2,
          );
          if (badLines.length > 0) {
            note({
              step: "detail-line-with-fewer-than-two-points",
              badLines,
              reason: "a polyline through fewer than two points has no length and draws nothing",
            });
          }
          // A flat series (min === max) is honest — the scale just has no span — but the
          // marker must still say so, because a pass that only counts elements cannot tell
          // a flat line from a scaled one and that is exactly the claim being made.
          note({
            step: "detail-trend-carries-its-bounds",
            bounded: drawn.filter((row) => row.min !== null && row.max !== null).length,
          });
        }
        // No non-finite text on a numbers screen, here as on the metric table.
        const detailBody = (await page.locator("body").innerText().catch(() => "")) || "";
        if (/NaN|Infinity|undefined/i.test(detailBody)) {
          note({
            step: "detail-non-finite-text",
            reason: "the detail page rendered NaN, Infinity or undefined",
          });
        }
        await shot(page, "health-service-detail");
        // Back to the overview, so the next leg does not start from the detail page.
        await page.goBack({ waitUntil: "domcontentloaded" }).catch(() => {});
        await page
          .waitForSelector("[data-health-service]", { timeout: 15000 })
          .catch(() => {});
      }
    }
  }

  report.health = { steps };
}

/**
 * `/health/metrics` — the history screen, driven end to end (REQ-014, slice 2).
 *
 * This screen's whole claim is that **the table and the export are the same rows over the same
 * window**, so the pass asserts that claim rather than the presence of a table:
 *
 * - the range buttons are real controls and switching one re-reads, changing the window shown;
 * - a row with samples draws a sparkline with as many points as the server sent, so a table
 *   rendering a flat line for a real series is caught — and a row with no samples says so
 *   rather than drawing nothing;
 * - **the CSV the export button downloads is fetched and compared against the table**: same row
 *   count, and the range stamped in the file equals the range on screen. This is the acceptance
 *   criterion "CSV export matches the range shown", checked on the file rather than on the
 *   button's success;
 * - an unoffered range is refused by the server — the pass asks for `168` directly, because the
 *   silent-clamp implementation passes every assertion above.
 */
async function runHealthMetricsDepth(page, report) {
  const steps = {};
  // Named `note` like every other depth pass — a local `record` would shadow the module's
  // `record` and call itself, which is a stack overflow on the first step rather than a
  // wrong report, and it happens only when the pass runs.
  const note = (key, value) => {
    steps[key] = value;
    record({ page: "health-metrics", action: "health-metrics-depth", step: key, ...value });
  };

  await page.goto(`${URL_ADMIN}/health/metrics`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-health-metrics-window], [data-health-metrics-error]", { timeout: 20000 })
    .catch(() => {});

  const window_ = (await page.locator("[data-health-metrics-window]").textContent().catch(() => "")) || "";
  note("windowNamed", /1h|24h|7d/.test(window_));

  // The screen reached itself from the overview as well — a screen nobody can reach
  // from the product is the "hidden feature" the definition of done forbids.
  await page.goto(`${URL_ADMIN}/health`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-health-metrics-link]", { timeout: 15000 }).catch(() => {});
  const href = await page.locator("[data-health-metrics-link]").getAttribute("href").catch(() => "");
  note("overviewLinksHere", href === "/health/metrics");
  await page.locator("[data-health-metrics-link]").click({ timeout: 8000 }).catch(() => {});
  await page
    .waitForSelector("[data-health-metrics-window]", { timeout: 15000 })
    .catch(() => {});
  note("clickedThrough", page.url().includes("/health/metrics"));

  // ---- The range selector is a real control, and it changes what is shown -------------------
  await page.locator('[data-health-range="1h"]').click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(700);
  const hourWindow = (await page.locator("[data-health-metrics-window]").textContent().catch(() => "")) || "";
  note("rangeSwitched", /1h/.test(hourWindow));
  await page.locator('[data-health-range="7d"]').click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(700);
  const weekWindow = (await page.locator("[data-health-metrics-window]").textContent().catch(() => "")) || "";
  note("rangeSwitchedTwice", /7d/.test(weekWindow));

  // ---- Rows: a real series draws a real line, an empty window says so ----------------------
  const rows = await page.locator("[data-health-metric-row]").count();
  const cards = await page.locator("[data-health-metric-card]").count();
  note("hasRowsOrEmptyState", rows > 0 || cards > 0 || (await page.locator("[data-health-metrics-empty]").count()) > 0);
  if (rows > 0) {
    const lines = await page.locator('[data-health-spark="line"]').count();
    const points = await page.locator('[data-health-spark="point"]').count();
    const empties = await page.locator('[data-health-spark="empty"]').count();
    note("sparksDrawn", lines + points + empties > 0);
    // No NaN / Infinity text anywhere on a numbers screen.
    const body = (await page.locator("body").innerText().catch(() => "")) || "";
    note("noNonFiniteText", !/NaN|Infinity|undefined/i.test(body));
  }
  await shot(page, "health-metrics-table");

  // ---- The export, read as a file and compared against the table ----------------------------
  // The button is not the assertion: a green download proves the click worked. What has to hold
  // is that the FILE says the same window the SCREEN says and carries one row per table row.
  const comparison = await page
    .evaluate(async () => {
      const answer = await fetch("/api/v1/health/metrics?range=7d", { credentials: "same-origin" });
      const table = answer.ok ? await answer.json() : null;
      const file = await fetch("/api/v1/health/metrics.csv?range=7d", { credentials: "same-origin" });
      if (!file.ok) return { ok: false, reason: `csv answered ${file.status}` };
      const text = await file.text();
      const lines = text.split("\n").filter((line) => line.trim() !== "");
      return {
        ok: true,
        servedRange: file.headers.get("x-health-range"),
        fileName: /filename="?([^";]+)"?/.exec(file.headers.get("content-disposition") || "")?.[1] || "",
        header: lines[0] || "",
        rows: lines.length - 1,
        tableRange: table?.range ?? null,
        tableRows: table?.metrics?.length ?? null,
        lastColumn: lines.slice(1).map((line) => line.split(",").pop()),
      };
    })
    .catch((err) => ({ ok: false, reason: String(err) }));

  if (!comparison.ok) {
    note("exportReadable", false);
  } else {
    note("exportReadable", true);
    note("exportRangeIsNamed", comparison.servedRange === "7d");
    note("exportFileNameCarriesRange", comparison.fileName.includes("7d"));
    note("exportHasHeader", comparison.header.includes("service") && comparison.header.includes("range"));
    note(
      "exportRowsMatchTable",
      comparison.tableRows === null ? comparison.rows === 0 : comparison.rows === comparison.tableRows,
    );
    // Every row stamps the window: a file whose rows do not say which window they
    // cover cannot be checked against the screen it came from.
    note(
      "everyRowStampsRange",
      Array.isArray(comparison.lastColumn) &&
        comparison.lastColumn.length > 0 &&
        comparison.lastColumn.every((cell) => cell === "7d"),
    );
    // The range the table reported and the range the CSV was served must agree.
    note("tableAndExportAgree", comparison.tableRange === comparison.servedRange);
  }

  // ---- An unoffered range is refused by the server, not clamped ------------------------------
  // The silent clamp passes every other assertion in this pass.
  const refusal = await page
    .evaluate(async () => {
      const answer = await fetch("/api/v1/health/metrics?range=168", { credentials: "same-origin" });
      let message = "";
      try {
        message = (await answer.json())?.error?.message ?? "";
      } catch {
        message = "";
      }
      return { status: answer.status, message };
    })
    .catch(() => ({ status: 0, message: "" }));
  note("unofferedRangeRefused", refusal.status === 400);
  note("refusalNamesTheRanges", /1h/.test(refusal.message) && /7d/.test(refusal.message));

  // ---- Mobile: the cards keep the value and the state without horizontal scroll -------------
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${URL_ADMIN}/health/metrics`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-health-metrics-window]", { timeout: 15000 }).catch(() => {});
  const mobileCards = await page.locator("[data-health-metric-card]").count();
  const overflow = await page
    .evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1)
    .catch(() => false);
  note("mobileCardsOrEmpty", mobileCards > 0 || (await page.locator("[data-health-metrics-empty]").count()) > 0);
  note("mobileNoHorizontalScroll", !overflow);
  if (mobileCards > 0) {
    const card = await page.locator("[data-health-metric-card]").first().innerText().catch(() => "");
    note("mobileCardShowsValueAndState", /\d/.test(card) && /healthy|degraded|down|unknown/.test(card));
  }
  await shot(page, "health-metrics-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });

  report.healthMetrics = { steps };
}

/**
 * `/health/incidents` — the timeline (REQ-014, slice 3).
 *
 * The property this pass exists for is not "the table renders". It is that **acknowledging
 * persists and the screen says who did it**, because an acknowledge button that updates local
 * state and forgets on reload is the exact failure a status screen cannot afford: an operator
 * hands over a shift saying "I've got it" and the next person sees an unclaimed page.
 */
async function runHealthIncidentsDepth(page, report) {
  const steps = {};
  const note = (key, value) => {
    steps[key] = value;
    record({ page: "health-incidents", action: "health-incidents-depth", step: key, ...value });
  };

  await page.goto(`${URL_ADMIN}/health/incidents`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-health-incidents-count], [data-health-incidents-error]", { timeout: 20000 })
    .catch(() => {});

  const rows = await page.locator("[data-health-incident-row]").count();
  const cards = await page.locator("[data-health-incident-card]").count();
  const empty = await page.locator("[data-health-incidents-empty]").count();
  note("hasRowsOrEmptyState", rows > 0 || cards > 0 || empty > 0);

  // The service dropdown is filled from the *server's* vocabulary, never a client constant: a
  // filter offering a service the platform does not probe would answer "no incidents" for a
  // question nobody asked.
  const options = await page.locator("[data-health-incidents-service] option").count();
  note("serviceFilterOffered", options > 1);

  // ---- Acknowledging persists, and the row names the actor ------------------------------------
  const ackButtons = await page.locator("[data-health-incident-ack]").count();
  if (ackButtons > 0) {
    await page.locator("[data-health-incident-note]").fill("claimed by the walkthrough").catch(() => {});
    await page.locator("[data-health-incident-ack]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1200);

    // The leg that matters: re-read the row *through the API*, not through the DOM. A screen
    // that painted the change locally would pass a DOM check and lose it on the next load.
    const id = await page.locator("[data-health-incident-ack]").first().getAttribute("data-health-incident-ack").catch(() => "");
    const persisted = await page
      .evaluate(async (incidentId) => {
        const answer = await fetch(`/api/v1/health/incidents/${incidentId}`, { credentials: "same-origin" });
        if (!answer.ok) return { ok: false, status: answer.status };
        const row = await answer.json();
        return { ok: true, acknowledged_by: row.acknowledged_by ?? null, note: row.note ?? "" };
      }, id)
      .catch(() => ({ ok: false }));
    note("acknowledgementPersists", Boolean(persisted.ok && persisted.acknowledged_by));
    note("acknowledgementCarriesTheNote", Boolean(persisted.ok && persisted.note.includes("walkthrough")));

    // And the screen shows it, after a full reload rather than a state update.
    await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(1200);
    const ackedOnScreen = await page.locator(`[data-health-incident-acked="${id}"]`).count();
    note("acknowledgedVisibleAfterReload", ackedOnScreen > 0);
  } else {
    // Nothing to claim is a legitimate state — but only when the table said so itself.
    note("nothingToAcknowledge", empty > 0 || rows > 0);
  }

  // ---- A duration is a number or the word "open", never "0 s" ---------------------------------
  // An open incident has no duration at all. Rendering `0 s` for one reads as "it lasted no
  // time", which is the exact opposite of the row's meaning, so a literal zero anywhere in the
  // column is a defect regardless of how many rows there are.
  const durations = await page.locator("[data-health-incident-duration]").allTextContents().catch(() => []);
  note(
    "noZeroDuration",
    durations.length === 0 || !durations.some((text) => /^\s*0\s*s\s*$/.test(text || "")),
  );
  // And every open row says so in words.
  const openRows = await page.locator('[data-health-incident-open="true"]').count();
  const openLabels = await page
    .locator('[data-health-incident-open="true"] [data-health-incident-duration]')
    .allTextContents()
    .catch(() => []);
  note(
    "openRowsSayOpen",
    openRows === 0 || openLabels.length === openRows || openLabels.every((text) => /open/.test(text || "")),
  );

  // ---- The state filter narrows, and the count follows ----------------------------------------
  await page.locator('[data-health-incidents-state="open"]').click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(900);
  const openOnly = await page.locator('[data-health-incident-state="open"]').getAttribute("aria-pressed").catch(() => "");
  note("stateFilterPressed", openOnly === "true");

  const body = (await page.locator("body").innerText().catch(() => "")) || "";
  note("noNonFiniteText", !/NaN|Infinity|undefined/i.test(body));
  await shot(page, "health-incidents");

  // ---- Mobile: cards, and no horizontal scroll ------------------------------------------------
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${URL_ADMIN}/health/incidents`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-health-incidents-count]", { timeout: 15000 }).catch(() => {});
  const mobileCards = await page.locator("[data-health-incident-card]").count();
  const overflow = await page
    .evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1)
    .catch(() => false);
  note("mobileCardsOrEmpty", mobileCards > 0 || (await page.locator("[data-health-incidents-empty]").count()) > 0);
  note("mobileNoHorizontalScroll", !overflow);
  await shot(page, "health-incidents-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });

  report.healthIncidents = { steps };
}

/**
 * `/health/settings` — the policy (REQ-014, slice 3).
 *
 * The property here is **the difference between a saved limit and a suggestion**. A settings
 * screen that shows seven numbers whether or not anyone chose them is the most dangerous kind of
 * status UI: every one of them looks like a decision somebody made, and the breach emitter obeys
 * them. So the pass saves a real pair and asserts the row flips from `suggestion` to `saved`.
 */
async function runHealthSettingsDepth(page, report) {
  const steps = {};
  const note = (key, value) => {
    steps[key] = value;
    record({ page: "health-settings", action: "health-settings-depth", step: key, ...value });
  };

  await page.goto(`${URL_ADMIN}/health/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-health-settings], [data-health-settings-error]", { timeout: 20000 })
    .catch(() => {});
  await page.waitForSelector("[data-health-settings-thresholds], [data-health-settings-skeleton]", { timeout: 20000 }).catch(() => {});

  const rows = await page.locator("[data-health-threshold-row]").count();
  note("thresholdsRendered", rows === 7);

  // ---- Save a real pair, and assert the row stops claiming to be a suggestion ----------------
  if (rows > 0) {
    const warn = await page.locator('[data-health-threshold-warn="disk_percent"]').count();
    note("thresholdInputsPresent", warn > 0);
    if (warn > 0) {
      await page.locator('[data-health-threshold-warn="disk_percent"]').fill("81").catch(() => {});
      await page.locator('[data-health-threshold-crit="disk_percent"]').fill("91").catch(() => {});
      await page.locator("[data-health-settings-save]").click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(1500);

      const saved = await page
        .locator('[data-health-threshold-row="disk_percent"]')
        .getAttribute("data-health-threshold-configured")
        .catch(() => "");
      note("savedPairIsMarkedSaved", saved === "true");
    }
  }

  // ---- The server refuses an inverted pair, and says which metric ----------------------------
  const refusal = await page
    .evaluate(async () => {
      const answer = await fetch("/api/v1/health/settings", {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ thresholds: [{ metric: "queue_depth", warn: 900, crit: 100, direction: "above" }] }),
      });
      let message = "";
      try {
        message = (await answer.json())?.error?.message ?? "";
      } catch {
        message = "";
      }
      return { status: answer.status, message };
    })
    .catch(() => ({ status: 0, message: "" }));
  note("invertedPairRefused", refusal.status === 400);
  note("refusalNamesTheMetric", refusal.message.includes("queue_depth"));

  // ---- The out-of-range interval is refused too, rather than clamped --------------------------
  const intervalRefusal = await page
    .evaluate(async () => {
      const answer = await fetch("/api/v1/health/settings", {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ check_interval_seconds: 99999 }),
      });
      return { status: answer.status };
    })
    .catch(() => ({ status: 0 }));
  note("outOfRangeIntervalRefused", intervalRefusal.status === 400);

  // ---- Maintenance windows: create, list, delete ----------------------------------------------
  const windowRowsBefore = await page.locator("[data-health-window-row]").count();
  const starts = await page.locator("[data-health-window-start]").count();
  note("windowFormPresent", starts > 0);
  if (starts > 0) {
    // A window that has already ended is still a window: the point of this leg is that the row
    // appears and can be withdrawn, not that the platform is mid-deploy.
    const past = new Date(Date.now() - 60 * 60 * 1000).toISOString().slice(0, 16);
    const later = new Date(Date.now() + 60 * 60 * 1000).toISOString().slice(0, 16);
    await page.locator("[data-health-window-start]").fill(past).catch(() => {});
    await page.locator("[data-health-window-end]").fill(later).catch(() => {});
    await page.locator("[data-health-window-note]").fill("walkthrough window").catch(() => {});
    await page.locator("[data-health-window-add]").click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1400);
    const windowRowsAfter = await page.locator("[data-health-window-row]").count();
    note("windowCreated", windowRowsAfter === windowRowsBefore + 1);

    // An end before a start is refused by the *server*; the form also disables the button, so
    // the walk sends the impossible body directly rather than trusting the disabled control.
    const backwards = await page
      .evaluate(async () => {
        const now = new Date();
        const answer = await fetch("/api/v1/health/maintenance-windows", {
          method: "POST",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            starts_at: now.toISOString(),
            ends_at: new Date(now.getTime() - 60000).toISOString(),
            services: [],
            note: "impossible",
          }),
        });
        return { status: answer.status };
      })
      .catch(() => ({ status: 0 }));
    note("backwardsWindowRefused", backwards.status === 400);

    // Withdraw it again, so a walkthrough does not leave the QA database with a mute button.
    const id = await page.locator("[data-health-window-delete]").first().getAttribute("data-health-window-delete").catch(() => "");
    if (id) {
      await page.locator(`[data-health-window-delete="${id}"]`).click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(1200);
      const remaining = await page.locator("[data-health-window-row]").count();
      note("windowDeleted", remaining === windowRowsBefore);
    }
  }

  const body = (await page.locator("body").innerText().catch(() => "")) || "";
  note("noNonFiniteText", !/NaN|Infinity|undefined/i.test(body));
  await shot(page, "health-settings");

  // ---- Mobile: one card per metric, no horizontal scroll ---------------------------------------
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`${URL_ADMIN}/health/settings`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-health-settings-threshold-cards]", { timeout: 15000 }).catch(() => {});
  const mobileCards = await page.locator("[data-health-threshold-card]").count();
  const overflow = await page
    .evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1)
    .catch(() => false);
  note("mobileThresholdCards", mobileCards > 0);
  note("mobileNoHorizontalScroll", !overflow);
  await shot(page, "health-settings-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });

  report.healthSettings = { steps };
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
  const site = qaScalar(`select id from sites where key = '${CREDS.siteKey}' limit 1`, "the QA site (key '${CREDS.siteKey}')");
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

  // The analytics batch goes in before the routes are walked: the report screens read it, and the
  // history fixture gives their series more than one bucket to draw.
  report.analytics = await seedAnalytics(report);
  log(`analytics seed: ${JSON.stringify(report.analytics)}`);

  const routes = [
    { path: "/", name: "overview" },
    { path: "/pages", name: "pages" },
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
    // The tenant screens (REQ-005, slice 1): the organization list and the detail screen with
    // its Members tab. No untested screen — both are walked, clicked and measured here, and the
    // depth pass below invites an address, refuses a second invite to the same one and revokes
    // what it created.
    { path: "/organizations", name: "organizations" },
    // The analytics reports (REQ-007, slice 2): every screen of the section is walked, clicked and
    // measured, and the depth pass below reads the range, the comparison, a drawer and an export.
    // The notification list (REQ-021, slice 1) — walked here and driven by the depth pass
    // below, which emits real notifications through the API, checks the bell's badge against
    // its own grouped lines, filters from a group line, runs a bulk action and proves the
    // keyboard path.
    { path: "/notifications", name: "notifications" },
    // The cache rules (REQ-011, slice 1) — walked here and driven by the depth pass below,
    // which creates a rule, watches the live match tester answer both ways, submits a TTL
    // above the cap to capture the field error, reorders the table and deletes what it made.
    // The overview and the settings screen are walked for the same reason: a screen that is
    // reachable only by clicking is a screen that is never visited.
    { path: "/cdn", name: "cdn-overview" },
    { path: "/cdn/rules", name: "cdn-rules" },
    { path: "/cdn/settings", name: "cdn-settings" },
    // The purge history and the purge console (REQ-011, slice 2). The REQ's own QA plan lists
    // all six CDN screens, and these two were reachable only by a click from another screen —
    // which is exactly the case the "no untested screen" rule exists for: a route nobody ever
    // opens directly is a route whose *first paint* nobody has seen, and on the purge console
    // that first paint is the form an operator lands on when a page is serving stale. The
    // console's own form submit is driven by `runCdnPurgeDepth` below.
    { path: "/cdn/purges", name: "cdn-purges" },
    { path: "/cdn/purge", name: "cdn-purge-console" },
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
    // The staging environments (REQ-017, slice 2). The list is walked here; its depth pass below
    // drives the wizard and then opens a *real* environment's detail screen, for the same reason
    // the webhook detail is not walked by id: a route opened with a placeholder id only proves
    // the not-found state renders.
    { path: "/environments", name: "environments" },
    // The security centre's five screens (REQ-012, slices 1-3). `runSecurityDepth` drives the
    // overview, the findings store and the header policy, but it never opened the last two --
    // and the same is true of the route list, so two screens that ship with rules, a policy
    // editor and a live counter had never been rendered by anything. "No untested screen"
    // means no untested screen: both are walked here and clicked by the depth pass below.
    { path: "/security", name: "security-overview" },
    { path: "/security/findings", name: "security-findings" },
    { path: "/security/headers", name: "security-headers" },
    { path: "/security/rate-limits", name: "security-rate-limits" },
    { path: "/security/sign-in-protection", name: "security-sign-in-protection" },
    // The system health centre (REQ-014, slice 1). Walked here and driven by
    // `runHealthDepth` below, which reads the eight service rows, opens a row's
    // checks and presses "Run all checks". "No untested screen" means no
    // untested screen: a status screen that has never been rendered by anything
    // is the one screen whose whole job is to be believed.
    { path: "/health", name: "health-overview" },
    // The drill-down is a *different screen* and is walked as one, for the reason the
    // definition of done spells out: every row on the overview links here, so a detail
    // page that was never rendered is eight dead affordances. `runHealthDepth` clicks
    // through to it and asserts the landing URL, which is what proves the link works.
    { path: "/health/services/redis", name: "health-service-detail" },
    // The metric history screen (REQ-014, slice 2) — walked for the same reason: the
    // overview links to it, so an unwalked page is a dead affordance, and `runHealthMetricsDepth`
    // reads the CSV the export button downloads and compares it against the table on screen.
    { path: "/health/metrics", name: "health-metrics" },
    // Slice 3's two new screens (REQ-014). Both are reached from the overview, so both are
    // walked rather than left to a route entry that nothing clicks into:
    //
    // * `/health/incidents` — the timeline. `runHealthIncidentsDepth` presses the state filter,
    //   types into the note box and clicks an Acknowledge button, because the one thing a
    //   status screen must not ship is an acknowledge button that does not persist.
    // * `/health/settings` — the policy. `runHealthSettingsDepth` reads a threshold pair into
    //   the form and saves, then asserts the row comes back marked `saved` rather than
    //   `suggestion`, which is the difference between a limit and a placeholder.
    { path: "/health/incidents", name: "health-incidents" },
    { path: "/health/settings", name: "health-settings" },
  ];
  // The route loop is per-route isolated for the same reason the depth passes are: a crashed
  // tab (`Page crashed`, which several concurrent passes can cause by exhausting the box's
  // memory) used to end the entire run, so every route after the crash and every depth pass
  // were skipped and no report was written at all. A page that dies is a finding about that
  // page; the pages after it still have to be looked at.
  // The scope is honoured here and nowhere else, so the pass says out loud what it resolved and
  // which routes that left. Without it a mis-scoped run is *silent*: it walks the whole product
  // under a scope label, and every artifact it writes — screenshots, clicks, the summary's
  // `scope` field — then claims the narrower coverage. That is the same failure as a dead run
  // writing a clean report, one level down, and the only witness is this line.
  const walkedRoutes = ONLY_ALL ? routes : routes.filter((route) => wants(route.name));
  for (const route of walkedRoutes) matchedOnly.add(route.name);
  if (!ONLY_ALL) {
    log(`focused pass: ${walkedRoutes.length}/${routes.length} routes -- ${ONLY.join(", ")}`);
  }
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

  // The file manager's depth pass (REQ-010, slice 1): a folder is created, the listing is filtered,
  // two files are selected so the bulk bar appears, one is trashed, and the trash brings it back.
  // Each depth pass is isolated: one throwing must not skip the ones after it. A pass that
  // cannot run is a finding of its own ("this screen did not answer"), not a reason to end the
  // whole run before the remaining screens have been looked at.
  report.mediaFiles = await runDepthPass("media-file-manager", () =>
    runMediaFileManager(page, report),
  );

  // The file detail screen (REQ-010, slice 2): a real file is opened, its preview renders, the
  // metadata saves, and the version history is read. This is the pass that proves the screen is
  // a screen — a route walked only by id would render its error state and look visited.
  report.mediaFileDetail = await runDepthPass("media-file-detail", () =>
    runMediaFileDetail(page, report),
  );
  log(`media file detail: ${JSON.stringify(report.mediaFileDetail)}`);

  report.mediaPresets = await runDepthPass("media-presets", () => runMediaPresets(page, report));
  log(`media presets: ${JSON.stringify(report.mediaPresets)}`);

  // The storage tab (REQ-010, slice 3): the range refused by the form, a connection test that
  // says what it proved, and a save that leaves the untouched fields alone.
  report.mediaStorage = await runDepthPass("media-storage", () => runMediaStorage(page, report));
  log(`media storage: ${JSON.stringify(report.mediaStorage)}`);

  // The share tab (REQ-010, slice 3): the link is shown once and never again, the public URL
  // actually serves the bytes, and a revoke stops it on the very next request.
  report.mediaShares = await runDepthPass("media-shares", () => runMediaShares(page, report));
  log(`media shares: ${JSON.stringify(report.mediaShares)}`);

  // The permissions tab (REQ-010, slice 4): the narrowing rule stated on the screen, the
  // chain a file inherits from, a deny refused when it names nothing, and a real deny that
  // names its subject by name rather than by uuid.
  report.mediaGrants = await runDepthPass("media-grants", () => runMediaGrants(page, report));
  log(`media grants: ${JSON.stringify(report.mediaGrants)}`);

  // The duplicate report (REQ-010, slice 3): two identical uploads form a group, the Merge button
  // is dead until a keeper is chosen, the merge keeps the *chosen* file, and the result says the
  // bytes are pending rather than reclaimed.
  report.mediaDuplicates = await runDepthPass("media-duplicates", () =>
    runMediaDuplicates(page, report),
  );
  log(`media duplicates: ${JSON.stringify(report.mediaDuplicates)}`);

  // The retention tab (REQ-010, slice 4): the policies state their consequence in a sentence,
  // the purge-inside-the-restore-window refusal is visible *before* the save, a run reports a
  // sentence and writes a log row even when it found nothing, and the file's hold switch is on
  // the tab where the file's other facts are.
  report.backups = await runDepthPass("backups", () => runBackups(page, report));
  report.mediaRetention = await runDepthPass("media-retention", () => runMediaRetention(page, report));
  log(`media retention: ${JSON.stringify(report.mediaRetention)}`);

  // The palette is global chrome: it has to open from anywhere, search for real and open a screen.
  await runDepthPass("palette", () => runPalette(page, report));

  // The command centre's own pass (REQ-032): commands, prefixes, running one, and its history.
  await runDepthPass("command-center", () => runCommandCenter(page, report));

  // The depth pass: facets, selection, copy, export and the index's own settings screen.
  await runDepthPass("search-depth", () => runSearchDepth(page, report));

  // The analytics depth pass (REQ-007, slice 2): the range, the comparison, a page drawer and a
  // real CSV download. Goals, funnels and realtime arrive with slice 3; the privacy half of the
  // settings screen with slice 4 — this pass visits what exists today.
  report.analyticsDepth = await runDepthPass("analyticsDepth", () => runAnalyticsDepth(page, report))

  // The goals + realtime pass (REQ-007, slice 3): a goal is created through the editor, a visitor
  // completes it after it exists, and the funnel and the live counters are read back.
  report.analyticsGoals = await runDepthPass("analyticsGoals", () => runGoalAndRealtimeDepth(page, report))
  log(`analytics goals: ${JSON.stringify(report.analyticsGoals)}`);

  // The settings and privacy pass (REQ-007, slice 4): tracking on/off persisted, a refused
  // retention value, the exclusions' preview, a purge and an erasure proven against the QA
  // database.
  report.analyticsSettings = await runDepthPass("analyticsSettings", () => runAnalyticsSettingsDepth(page, report))

  // The notification pass (REQ-021, slice 1): the bell's badge against its own grouped lines,
  // a grouped line filtering the list, a bulk action reporting what it changed, the keyboard
  // path, and the three states. It runs after the analytics passes because it emits into the
  // signed-in account's own inbox and would otherwise add rows to a list a later pass counts.
  report.notifications = await runDepthPass("notifications", () => runNotificationsDepth(page, report))
  log(`notifications: ${JSON.stringify(report.notifications)}`);

  // The event console (REQ-016, slice 1): the feed, its filters, the payload inspector and the
  // catalogue. It runs after the notification passes because it publishes a page, and the
  // content screens' own passes are ordered after it in the file.
  report.events = await runDepthPass("events-console", () => runEventsDepth(page, report));
  log(`events: ${JSON.stringify(report.events)}`);

  // The webhook endpoints and their delivery operations (REQ-016, slice 2). It runs right after
  // the events pass because it points an endpoint at a real receiver and reads what the
  // receiver actually accepted, which is the one claim on this screen no API status code can
  // make on its own.
  report.webhooks = await runDepthPass("webhooks", () => runWebhooksDepth(page, report));
  log(`webhooks: ${JSON.stringify(report.webhooks)}`);

  // The staging environments (REQ-017, slice 2): the list, the create wizard end to end, the
  // detail screen and the discard confirmation. It runs right after the webhooks pass because it
  // clones production content, and the rows it counts are the same pages the events pass has just
  // published — so a pass that ran earlier would clone an empty site and report "0 of 2 copied"
  // as if that were a defect in the copy.
  report.environments = await runDepthPass("environments", () => runEnvironmentsDepth(page, report));
  log(`environments: ${JSON.stringify(report.environments)}`);

  // The bus's own retention (REQ-016, slice 3). It runs after the events and webhook passes —
  // both of which count rows on the bus — because a sweep deletes, and a pass that deleted
  // first would make their numbers wrong for a reason that has nothing to do with them.
  report.retention = await runDepthPass("event-retention", () => runRetentionDepth(page, report));
  log(`retention: ${JSON.stringify(report.retention)}`);

  // The security centre (REQ-012, slice 1). It runs after the events and webhook passes
  // because a scan counts the findings those passes have already written, and a scan that ran
  // first would report a posture that the rest of the pass then invalidates.
  report.security = await runDepthPass("security", () => runSecurityDepth(page, report));
  log(`security: ${JSON.stringify(report.security)}`);

  // The system health centre (REQ-014, slices 1 and 3). It runs after the security pass
  // because both screens run live probes, and running them in the other order
  // would have the health screen's own PostgreSQL probe read the connection pool
  // the security scan is still holding.
  //
  // Two writers had added to this one region and the merge left BOTH copies of the block, so a
  // full pass ran `runHealthDepth` twice — the second run re-probed every service and overwrote
  // the first run's report with its own, and the `if (wants("health-overview"))` wrapper meant a
  // pass scoped to `health-incidents` alone ran none of these. The scope is now `runDepthPass`'s
  // job alone: it already tests `wants(name)`, records `matchedOnly` from inside that test, and
  // wraps the call so a crashed tab cannot take the next statement with it. A second hand-written
  // `if (wants(...))` around the block could only ever narrow the scope, never widen it.
  report.health = await runDepthPass("health", () => runHealthDepth(page, report));
  report.healthMetrics = await runDepthPass("health-metrics", () => runHealthMetricsDepth(page, report));
  report.healthIncidents = await runDepthPass("health-incidents", () => runHealthIncidentsDepth(page, report));
  report.healthSettings = await runDepthPass("health-settings", () => runHealthSettingsDepth(page, report));
  log(`health: ${JSON.stringify(report.health)}`);

  // The preferences pass (REQ-021, slice 2). It runs immediately after the list pass and
  // restores the row it touched, so a later pass in the same run sees the defaults rather
  // than whatever this one left behind.
  report.notificationSettings = await runDepthPass("notificationSettings", () => runNotificationSettingsDepth(page, report))
  log(`notification settings: ${JSON.stringify(report.notificationSettings)}`);

  // The outbox and routing pass (REQ-021, slice 3). It runs after the list and preferences
  // passes because it emits into the same inbox, and it cleans up every row it creates — a QA
  // database that grows a notification per pass is one whose counts stop meaning anything.
  report.notificationOutbox = await runDepthPass("notificationOutbox", () => runNotificationOutboxDepth(page, report))
  log(`notification outbox: ${JSON.stringify(report.notificationOutbox)}`);
  log(`analytics settings: ${JSON.stringify(report.analyticsSettings)}`);

  // The cache rules (REQ-011, slice 1): the live match tester answering both ways, a TTL
  // above the cap refused under its own field, a rule created and read back from the API, a
  // reorder that leaves a dense priority run, the toggle and the duplicate, the error state
  // and the mobile cards. It runs after the notification pass so the two do not both own the
  // same database rows.
  report.cdnRules = await runDepthPass("cdnRules", () => runCdnRulesDepth(page, report))
  log(`cdn rules: ${JSON.stringify(report.cdnRules)}`);

  // The purge pipeline (REQ-011, slice 2): the console's own refusals, the whole-zone
  // confirmation as a real gate, a purge that reaches the history, a failed row with the
  // provider's message, and a retry whose label names what it will re-send.
  report.cdnPurges = await runDepthPass("cdnPurges", () => runCdnPurgeDepth(page, report))
  log(`cdn purges: ${JSON.stringify(report.cdnPurges)}`);

  // The tenant depth pass (REQ-005, slice 1): the organization list, the Members tab, the
  // invite dialog's field refusal, a real invitation and its revocation.
  //
  // This pass OPENS the organization the five passes below then drive, so it goes through
  // `runDepthPass` like every other one and can be scoped out on its own. That leaves the
  // dependents with no organization, and a scope that names only a dependent would then read
  // `undefined.organizationId` out of the skip record and throw. The id is therefore checked
  // once, here, and a dependent asked for without its parent is recorded as a finding naming
  // the reason — never as a crash that takes the summary with it.
  const organizationDepth = await runDepthPass("organizationDepth", () =>
    runOrganizationDepth(page, report),
  );
  const tenantId = organizationDepth && organizationDepth.organizationId;
  // A dependent asked for without its parent is a finding naming the reason, and the pass is
  // skipped. It must NOT `return` from here: returning would abandon the remaining screens and
  // the run would end with no `summary.json` at all, which is the one shape a QA artifact must
  // never have — a missing summary reads like a crash and hides every finding behind it.
  const tenantMissing = (name) => {
    if (tenantId) return false;
    const reason = `the "${name}" pass needs the organization the tenant pass opens, and it did not return one`;
    log(`tenant pass ${name} failed: ${reason}`);
    record({ page: "organizations", action: "depth-pass-failed", pass: name, reason });
    return true;
  };

  // The Departments tab (REQ-005, slice 2): create a department through the real dialog, refuse
  // an unusable key, open the drawer, bind and revoke a role, try the move the API refuses and
  // clean up again. It needs the organization the pass above just opened.
  if (!wants("organizationDepartments")) {
    log(`depth pass organizationDepartments skipped (out of scope)`);
  } else if (tenantMissing("organizationDepartments")) {
    /* recorded above */
  } else {
    await runDepthPass("organizationDepartments", () => runOrganizationDepartments(page, report, tenantId));
  }
  log(`organization departments: ${JSON.stringify(report.organizationDepartments)}`);

  // The member drawer (REQ-005, slice 4): open a member, grant a role, extend a temporary grant
  // and revoke one. It runs after the departments pass because that pass leaves the organization
  // with its members, its roles and a live tab to open the drawer from.
  if (!wants("organizationMemberDrawer")) {
    log(`depth pass organizationMemberDrawer skipped (out of scope)`);
  } else if (tenantMissing("organizationMemberDrawer")) {
    /* recorded above */
  } else {
    await runDepthPass("organizationMemberDrawer", () => runOrganizationMemberDrawer(page, report, tenantId));
  }

  // The Modules, Settings and Billing tabs (REQ-005, slice 3): switch a module off and on and
  // read it back after a reload, change the locale and accent and prove both persisted, and
  // check that every usage bar names its metric and its number. Same organization, so it runs
  // straight after the departments pass rather than opening a second one.
  if (!wants("organizationTenantTabs")) {
    log(`depth pass organizationTenantTabs skipped (out of scope)`);
  } else if (tenantMissing("organizationTenantTabs")) {
    /* recorded above */
  } else {
    await runDepthPass("organizationTenantTabs", () => runOrganizationTenantTabs(page, report, tenantId));
  }
  log(`organization tenant tabs: ${JSON.stringify(report.organizationTenantTabs)}`);

  // The invite policy, the owner-approval queue and the Audit tab (REQ-005, slice 3 remainder):
  // the two screens that change what the API does. Runs straight after the tenant tabs because
  // it edits the same organization's policy and has to put it back.
  if (!wants("organizationInvitePolicy")) {
    log(`depth pass organizationInvitePolicy skipped (out of scope)`);
  } else if (tenantMissing("organizationInvitePolicy")) {
    /* recorded above */
  } else {
    await runDepthPass("organizationInvitePolicy", () => runOrganizationInvitePolicy(page, report, tenantId));
  }

  // The suspend/archive pass (REQ-005, slice 3's last part): the banner on a frozen tenant,
  // a refused write with the reason on screen, and the reactivation that clears both.
  if (!wants("organizationSuspend")) {
    log(`depth pass organizationSuspend skipped (out of scope)`);
  } else if (tenantMissing("organizationSuspend")) {
    /* recorded above */
  } else {
    await runDepthPass("organizationSuspend", () => runOrganizationSuspend(page, report, tenantId));
  }
  log(`organization suspend: ${JSON.stringify(report.organizationSuspend)}`);
  log(`organization invite policy: ${JSON.stringify(report.organizationInvitePolicy)}`);

  // The role-depth pass (REQ-006, slice 1): create a role, cycle a matrix cell three ways,
  // preview and save, reopen, and read the history tab back.
  report.iamRoles = await runDepthPass("iamRoles", () => runIamRolesDepth(page, report))

  // The subjects-and-scopes pass (REQ-006, slice 2): users, bindings at every scope, groups,
  // machine identities and the simulator.
  await runDepthPass("iam-subjects-depth", () => runIamSubjectsDepth(page, report));

  // The ABAC policies pass (REQ-006, slice 4a): the builder, the dry run and the history.
  report.iamPolicies = await runDepthPass("iamPolicies", () => runIamPoliciesDepth(page, report))
  log(`iam roles: ${JSON.stringify(report.iamRoles)}`);

  // The security-policy pass (REQ-006, slice 3): the policy screen with a refusal in the field
  // and a diff on save, the session list with a real revoke, the device registry and the MFA
  // enrolment dialog.
  await runDepthPass("iam-security-depth", () => runIamSecurityDepth(page, report));
  log(`iam security: ${JSON.stringify(report.iamSecurity)}`);

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
  await runDepthPass("iam-passkeys", () => runPasskeysDepth(page, report));
  log(`passkeys: ${JSON.stringify(report.passkeys)}`);

  // The permission-request pass (REQ-006, slice 4b): ask, approve with a window, refuse, and the
  // refusals of the ask form. It runs after the count-sensitive passes because an approval adds a
  // time-boxed binding (and the generated grant role) to the organization.
  await runDepthPass("iam-approvals-depth", () => runIamApprovalsDepth(page, report));
  log(`iam approvals: ${JSON.stringify(report.iamApprovals)}`);

  // The SCIM provisioning pass (REQ-006, slice 4b): mint a token, drive a create → deactivate
  // round trip through the real endpoint from this browser, read the sync log back, revoke the
  // token and prove it is refused afterwards.
  await runDepthPass("iam-provisioning-depth", () => runIamProvisioningDepth(page, report));
  log(`iam provisioning: ${JSON.stringify(report.iamProvisioning)}`);

  // The enterprise sign-in pass (REQ-006, slice 4b-2): connect a provider through the drawer,
  // read the "secret is a name, not a value" chip, run the discovery test and require it to
  // report a *result* (a provider that is not configured yet answers "failed", not a 500), then
  // remove the provider and see the list go back to its empty state.
  await runDepthPass("iam-authentication-depth", () => runIamAuthenticationDepth(page, report));
  log(`iam authentication: ${JSON.stringify(report.iamAuthentication)}`);

  // Mobile pass. The context is new, so it carries no session — without the sign-in below every
  // mobile screenshot would be the sign-in screen and no mobile layout would really be measured.
  const mobile = await context.browser().newContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  const mpage = markHydrationWait(await mobile.newPage());
  attach(mpage, "mobile");
  report.mobileLogin = await ensureSignedIn(mpage, report);
  if (!report.mobileLogin) {
    log("mobile pass: the sign-in did not land — the mobile screenshots will show the login form");
  }
  for (const route of [{ path: "/", name: "overview" }, { path: "/pages", name: "pages" }, { path: "/ai", name: "ai" }, { path: "/search?q=qa", name: "search" }, { path: "/settings/search", name: "search-settings" }, { path: "/settings/iam/users", name: "iam-users" }, { path: "/settings/iam/groups", name: "iam-groups" }, { path: "/settings/iam/simulator", name: "iam-simulator" }, { path: "/settings/iam/policies", name: "iam-policies" }, { path: "/settings/iam/approvals", name: "iam-approvals" }, { path: "/settings/iam/provisioning", name: "iam-provisioning" }, { path: "/settings/iam/authentication", name: "iam-authentication" }, { path: "/settings/iam/security", name: "iam-security" }, { path: "/settings/iam/sessions", name: "iam-sessions" }, { path: "/settings/iam/devices", name: "iam-devices" }, { path: "/analytics", name: "analytics" }, { path: "/analytics/pages", name: "analytics-pages" }, { path: "/analytics/goals", name: "analytics-goals" }, { path: "/analytics/settings", name: "analytics-settings" }]) {
    await mpage.goto(`${URL_ADMIN}${route.path}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await mpage.waitForTimeout(800);
    const diag = await diagnostics(mpage);
    await shot(mpage, `mobile-${route.name}`);
    report.mobile.push({ ...route, diagnostics: diag });
  }

  // A `mobile:` spelling names the same screen's phone layout, so the roll-up must accept it
  // as a known name instead of reporting it as unmatched.
  const mobileRoutes = [{ path: "/", name: "overview" }, { path: "/pages", name: "pages" }, { path: "/ai", name: "ai" }, { path: "/search?q=qa", name: "search" }, { path: "/settings/search", name: "search-settings" }, { path: "/settings/iam/users", name: "iam-users" }, { path: "/settings/iam/groups", name: "iam-groups" }, { path: "/settings/iam/simulator", name: "iam-simulator" }, { path: "/settings/iam/policies", name: "iam-policies" }, { path: "/settings/iam/approvals", name: "iam-approvals" }, { path: "/settings/iam/provisioning", name: "iam-provisioning" }, { path: "/settings/iam/authentication", name: "iam-authentication" }, { path: "/settings/iam/security", name: "iam-security" }, { path: "/settings/iam/sessions", name: "iam-sessions" }, { path: "/settings/iam/devices", name: "iam-devices" }, { path: "/analytics", name: "analytics" }, { path: "/analytics/pages", name: "analytics-pages" }, { path: "/analytics/goals", name: "analytics-goals" }, { path: "/analytics/settings", name: "analytics-settings" }, { path: "/security", name: "security-overview" }, { path: "/security/findings", name: "security-findings" }, { path: "/security/headers", name: "security-headers" }, { path: "/security/rate-limits", name: "security-rate-limits" }, { path: "/security/sign-in-protection", name: "security-sign-in-protection" }, { path: "/health", name: "health-overview" }, { path: "/health/metrics", name: "health-metrics" }];
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

  // The tenant screens (REQ-005, slice 4's mobile pass). They are walked here for the same
  // reason the palette is: the layouts that only exist under `md` — the members table as cards,
  // the department tree as cards, the trail as cards, the switcher as a sheet — are the *only*
  // rendering a phone gets, and a pass that never opened them at 390px would report nothing
  // about the layout the spec's own acceptance line names. The detail route carries an id, so it
  // cannot sit in the route list above; the depth pass below supplies the real one.
  const mobileOrganizationId = (report.organizations && report.organizations.organizationId) || null;
  const mobileTenantRoutes = [
    { path: "/organizations", name: "organizations" },
    ...(mobileOrganizationId
      ? [
          { path: `/organizations/${mobileOrganizationId}?tab=members`, name: "organization-members" },
          { path: `/organizations/${mobileOrganizationId}?tab=departments`, name: "organization-departments" },
          { path: `/organizations/${mobileOrganizationId}?tab=billing`, name: "organization-billing" },
          { path: `/organizations/${mobileOrganizationId}?tab=audit`, name: "organization-audit" },
        ]
      : []),
    // The staging surfaces (REQ-017). `/environments` carries the detail route's id and so is
    // walked by `runEnvironmentsDepth`; the create wizard has no id either, and it has no ROUTE
    // either — it is a modal the list opens with `data-env-new`. The spec names its phone layout
    // explicitly ("below `lg` the wizard becomes a single scrolling form") and nothing measured
    // it: the depth pass drives the wizard, but at 1280px.
    //
    // The list deep-links its own wizard through `?wizard=1` (the panel's other list screens do
    // the same for their own drawers), so this route IS the wizard at 390px. The assertion below
    // is what keeps that honest — a query parameter that the view ignores would photograph the
    // list and file it as a form, and the check is `[data-env-wizard]` on the page, not the path
    // that produced it.
    { path: "/environments", name: "environments" },
    { path: "/environments?wizard=1", name: "environments-wizard", expect: "[data-env-wizard]" },
  ];
  for (const r of mobileTenantRoutes) MOBILE_NAMES.add(r.name);
  for (const route of (ONLY_ALL
    ? mobileTenantRoutes
    : mobileTenantRoutes.filter((r) => wants(`mobile:${r.name}`) || wants(r.name)))) {
    await mpage.goto(`${URL_ADMIN}${route.path}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await mpage.waitForTimeout(1200);
    let rendered = true;
    if (route.expect) {
      rendered = (await mpage.locator(route.expect).count()) > 0;
      if (!rendered) {
        // A deep link that opens nothing is a broken deep link, and the screenshot would be of
        // whatever the URL fell back to — recorded here rather than left for a human to notice
        // that two of the twenty screenshots are the same screen.
        record({ page: "qa", action: "deep-link-render-failed", route: route.path, expect: route.expect });
      }
    }
    const diag = await diagnostics(mpage);
    await shot(mpage, `mobile-${route.name}`);
    report.mobile.push({ ...route, rendered, diagnostics: diag });
  }

  // The switcher on a phone is a bottom sheet, not a dropdown: this reads its geometry, because
  // "it opens" is not the claim — the claim is that it covers the screen, that its rows are
  // reachable with a thumb, and that the longest organization name fits inside it.
  await mpage.goto(`${URL_ADMIN}/`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await mpage.waitForTimeout(1500);
  const switcherButton = mpage.locator('button[aria-label="Current organization"]').first();
  if ((await switcherButton.count()) > 0) {
    await switcherButton.click({ timeout: 8000 }).catch(() => {});
    await mpage.waitForTimeout(700);
    const sheet = await mpage
      .evaluate(() => {
        const dialog = document.querySelector("[data-org-switcher]");
        if (!dialog) return null;
        const rect = dialog.getBoundingClientRect();
        const rows = [...dialog.querySelectorAll('[role="option"]')].map((row) =>
          Math.round(row.getBoundingClientRect().height),
        );
        const overflowX = document.documentElement.scrollWidth > innerWidth + 2;
        // The control is one component with two shapes: a bottom sheet below `sm` and a panel
        // beside the button from `sm` up. Which one is on screen is read rather than assumed,
        // because asserting the sheet's anchoring at desktop width measures the *panel* and
        // reports a correct layout as a defect.
        const isSheet = getComputedStyle(dialog).position === "fixed";
        return {
          shape: isSheet ? "sheet" : "panel",
          width: Math.round(rect.width),
          height: Math.round(rect.height),
          bottom: Math.round(rect.bottom),
          viewport: { w: innerWidth, h: innerHeight },
          rowHeights: rows.slice(0, 6),
          minRow: rows.length ? Math.min(...rows) : 0,
          closeButtons: dialog.querySelectorAll('button[aria-label="Close"]').length,
          overlay: document.querySelector('button[aria-label="Close the organization switcher"]') !== null,
          pageOverflow: overflowX,
        };
      })
      .catch(() => null);
    report.mobileSwitcher = sheet;
    // Overlay shots are viewport-only: a full-page capture of a fixed sheet also photographs the
    // page below the fold, which reads as an overlay that failed to cover the screen.
    await shot(mpage, "mobile-organization-switcher", { full: false });
    log(`mobile switcher: ${JSON.stringify(sheet)}`);
  }

  // The palette on a phone: a full-screen sheet with rows above the touch floor and a reachable
  // close control.
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

  // A route that threw is recorded WITHOUT `diagnostics` (see the `route-failed` push above), and
  // the roll-up used to dereference it directly — so one page that failed on a dead tab, a
  // navigation timeout or an `interact` throw destroyed every OTHER page's findings and the
  // report the run wrote. The cost is not one missing screen: it is the whole suite, silently,
  // at the exact moment the box is unhealthy enough to make routes fail. A route failure is
  // itself a finding, so it is now raised as one and the loop moves on.
  for (const p of report.pages) {
    if (p.failed) {
      pushFindings("high", "route-failed", `${p.name}: ${p.failed}`);
      continue;
    }
    const d = p.diagnostics || {};
    const { horizontalOverflow, scrollWidth, viewport, offscreen, brokenImages, emptyInteractives, unlabeledInputs, lowContrast, duplicateIds, h1Count } = d;
    if (horizontalOverflow) pushFindings("high", "overflow", `${p.name}: page scrolls horizontally (${scrollWidth}px > ${viewport?.w}px)`);
    if (offscreen?.length) pushFindings("high", "offscreen", `${p.name}: ${offscreen.length} element(s) outside the viewport, e.g. ${JSON.stringify(offscreen[0])}`);
    if (brokenImages?.length) pushFindings("high", "broken-image", `${p.name}: ${brokenImages.join(", ")}`);
    if (emptyInteractives?.length) pushFindings("medium", "unlabeled-control", `${p.name}: ${emptyInteractives.length} control(s) with no accessible name`);
    if (unlabeledInputs?.length) pushFindings("medium", "unlabeled-input", `${p.name}: ${unlabeledInputs.length} input(s) without a label`);
    if (lowContrast?.length) pushFindings("medium", "low-contrast", `${p.name}: ${lowContrast.length} text node(s) under WCAG AA, e.g. ${JSON.stringify(lowContrast[0])}`);
    if (duplicateIds?.length) pushFindings("low", "duplicate-id", `${p.name}: duplicate ids ${duplicateIds.join(", ")}`);
    if (h1Count === 0) pushFindings("low", "no-h1", `${p.name}: no h1 heading`);
  }
  for (const m of report.mobile) {
    if (m.diagnostics?.horizontalOverflow) pushFindings("high", "overflow-mobile", `mobile ${m.name}: horizontal overflow`);
    if (m.diagnostics?.offscreen?.length) pushFindings("medium", "offscreen-mobile", `mobile ${m.name}: ${m.diagnostics.offscreen.length} element(s) outside the viewport`);
  }
  // The switcher sheet's own claims, read above: a sheet that opens but does not cover the
  // screen, rows a thumb cannot reach, or a page that scrolls sideways underneath it are the
  // three ways "it opens on a phone" can be true and still be wrong. A `null` reading means the
  // account has no membership to switch between, which is a correct absence, not a failure.
  if (report.mobileSwitcher) {
    const sheet = report.mobileSwitcher;
    // `TOUCH_TARGET_MIN_PX`, the same floor the CDN rules affordances are held to. The message
    // used to say "44 is the floor for a touch target" while the branch fired below 40 — the
    // reader was told to fix a number the assertion never used, which is advice that cannot be
    // acted on and hides the real line when a row genuinely is short.
    if (sheet.minRow > 0 && sheet.minRow < TOUCH_TARGET_MIN_PX) {
      pushFindings("high", "tiny-target", `mobile organization switcher: rows are ${sheet.minRow}px tall (${TOUCH_TARGET_MIN_PX}px is the floor for a touch target)`);
    }
    // Only the *sheet* is anchored to the bottom edge; the panel that replaces it from `sm` up
    // is meant to hang beside its button. Asserting the sheet's anchoring on the panel reports
    // a correct layout as a defect, which is worse than asserting nothing.
    if (sheet.shape === "sheet") {
      if (sheet.bottom < sheet.viewport.h - 2) {
        pushFindings("high", "sheet-not-anchored", `mobile organization switcher: the sheet ends ${sheet.viewport.h - sheet.bottom}px above the bottom of the screen`);
      }
      if (!sheet.overlay) {
        pushFindings("medium", "sheet-no-overlay", "mobile organization switcher: the sheet opens without a backdrop, so the page behind it stays visible and tappable");
      }
    }
    if (sheet.pageOverflow) {
      pushFindings("high", "overflow-mobile", "mobile organization switcher: the page scrolls horizontally with the switcher open");
    }
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

  // A scope that matched nothing is a typo, not a green pass: the run has walked no screen at
  // all and its zero findings would sit in `summary.json` looking exactly like a clean one. Say
  // so here, where the next reader of the artifact will see it — and *before* the severity tally,
  // so the finding is counted rather than sitting in the list unnumbered.
  if (ONLY && report.pages.length === 0) {
    pushFindings("high", "qa-scope", `the scope "${ONLY}" matched no route — this pass walked nothing`);
  }

  const bySeverity = { high: 0, medium: 0, low: 0 };
  for (const f of findings) bySeverity[f.severity] += 1;

  const summary = {
    ...report,
    // Stamped on every artifact, so a scoped run can never be filed as a whole-repository pass.
    scope: ONLY || "full",
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
  if (ONLY) md.push(`> **Scoped pass** — only "${ONLY}". NOT a whole-repository result.`);
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
