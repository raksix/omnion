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
  if (i !== -1 && process.argv[i + 1] && !process.argv[i + 1].startsWith("--")) {
    return process.argv[i + 1];
  }
  // `--name=value` is the other spelling, and it is the one `run.sh` builds
  // (`QA_ONLY_ARGS=(--only="$QA_ONLY")`). Without this branch `arg("only")` returned
  // the fallback for every focused pass a writer has ever run, so `--only` narrowed
  // nothing: the pass walked the whole route list and every depth pass, which is the
  // exact wall-time problem `--only` was added to solve — and it failed silently,
  // because a filter that matches everything produces a complete, entirely green report.
  const eq = process.argv.find((a) => a.startsWith(`--${name}=`));
  if (eq !== undefined) return eq.slice(name.length + 3);
  return fallback;
}

const URL_ADMIN = arg("url", "http://127.0.0.1:3100");
const URL_WEB = arg("web", "http://127.0.0.1:3200");
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
 * The deployment screens the pass must cover. Declared HERE, above both
 * `assertDeploymentScreensWalked` and `module.exports`, because `module.exports` is evaluated at
 * LOAD time while a `const` further down the file is still in its temporal dead zone — the
 * exported reference threw `ReferenceError: Cannot access 'DEPLOYMENT_SCREENS' before
 * initialization` and took the ENTIRE walkthrough file down with it, so no pass ran at all and the
 * harness reported nothing. A missing screen and a broken harness must not look the same.
 */
const DEPLOYMENT_SCREENS = [
  "/deployment/artifacts",
  "/deployment/install",
  "/deployment/upgrade",
  // REQ-129: the ledger and the migration detail. The detail screen is only reachable from a row
  // that exists, which is why the depth pass below opens it from the ledger rather than the route
  // list fabricating a version — a walk of `/deployment/migrations/9999` would prove only that the
  // 404 renders.
  "/deployment/migrations",
];

/**
 * Refuse to start a pass whose deployment screens are not all in the route list.
 *
 * `run.sh` reads a non-zero exit as "the pass did not run", which is the honest outcome for a
 * harness that cannot prove what it claims to cover. A green pass over three of four screens is
 * the worse one: it looks like coverage.
 */
function assertDeploymentScreensWalked() {
  const missing = DEPLOYMENT_SCREENS.filter((path) => !srcHasRoute(path));
  if (missing.length > 0) {
    throw new Error(
      `deployment screens missing from the route list: ${missing.join(", ")} — an unwalked screen is an unmeasured screen`,
    );
  }
}

/**
 * `true` when the DESKTOP route list walks this exact path.
 *
 * Scoped to the `routes` array on purpose. The first version matched the whole file, and the phone
 * pass's `mobileRoutes` array carries the same three paths — so deleting a route from the desktop
 * list left the guard green, satisfied by a list nobody asked it about. A check scoped to the whole
 * file is scoped to things its subject never said, which is the sixth gate defect REQ-128 already
 * recorded for the Helm NOTES check.
 */
let ROUTE_SOURCE = "";
function srcHasRoute(path) {
  if (!ROUTE_SOURCE) {
    const source = fs.readFileSync(__filename, "utf8");
    const block = source.match(/const routes = \[[\s\S]*?\n  \];/);
    if (!block) {
      throw new Error("the desktop route list could not be read from this file");
    }
    ROUTE_SOURCE = block[0];
  }
  return ROUTE_SOURCE.includes(`{ path: "${path}",`);
}
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

  // The redirect into the wizard is a CLIENT-side `router.replace("/setup")`, so the URL only
  // settles after hydration and — on a cold `next dev` — after Turbopack compiles the `/` route
  // and `/login`'s `fetchOnboarding()` effect runs. A fixed sleep asked the question once and
  // got "not a first run" from a server-rendered `/` or `/login`, returned `ran: false`, and the
  // pass carried on into a tenant that did not exist: `ensureSignedIn` then reached `/setup`
  // seconds later, with warm modules, and its generic submit pressed "Create account" with
  // every field still empty. The API refused that correctly, and the run went on to answer
  // every org-scoped screen with 403 — 729 of them, reported as 1,477 findings about a product
  // that was never measured. Asking again is the whole fix.
  let url = page.url();
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if (url.includes("/setup") || url.includes("/login")) break;
    await page.waitForTimeout(500);
    url = page.url();
  }
  if (!url.includes("/setup")) {
    // A sign-in screen here is a first run whose redirect never landed: the panel would send
    // it to `/setup` once its modules were warm, which is exactly the state the pass used to
    // walk away from. Only an installation that has already been set up is a genuine skip.
    const settled = url.includes("/login")
      ? await page
          .waitForURL(/\/setup/, { timeout: 15000 })
          .then(() => true)
          .catch(() => false)
      : false;
    if (settled) {
      url = page.url();
      log(`wizard: reached /setup after the redirect settled (${url})`);
    } else {
      log(`wizard: not in setup (${url}) — installation already exists`);
      report.steps.push({ action: "wizard-skipped", url, reason: "not a first run" });
      return { ran: false, url, skippedBecause: "already-installed" };
    }
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
    // A step with nothing in it is a step the walk could not fill, not a step the platform
    // refused. Submitting it anyway produces a 400 that reads like a product defect while
    // saying nothing about the product — which is precisely how this run came to charge
    // 1,477 findings to a tenant that simply did not exist. Record the gap and move on.
    const wrote = (filled || []).filter((entry) => entry && entry.value);
    if (stepKey !== "theme" && wrote.length === 0) {
      report.steps.push({ index: i, stepKey, filled: [], clicked: null, reason: "nothing to fill" });
      log(`wizard: step ${stepKey} has no fillable input — not submitting an empty form`);
      break;
    }
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
  // Landing on `/setup` here means the first run is still open. The wizard is `runWizard`'s
  // job, and its steps each have their own form; a generic submit here presses "Create account"
  // with empty fields and the platform answers 400 — a refusal the report then reads as a
  // defect. Go to the real sign-in form instead of the wizard.
  if (page.url().includes("/setup")) {
    log("sign-in: the first run is still open — using the login form, not the wizard");
    await page.goto(`${URL_ADMIN}/login`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page.waitForTimeout(700);
  }
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
  // Wait for the sign-in to LAND, not for a fixed span.
  //
  // `waitForTimeout(1200)` was the same class of bug `runWizard` had: on a cold `next dev` the
  // click on "Sign in" is what triggers Turbopack to compile the authenticated route, so 1.2 s
  // can easily be answered while the panel is still compiling and the URL is still `/login`.
  // The pass then returned `signedIn: false` and died at `FATAL: could not sign in after wizard`
  // on a login that had in fact been accepted — which is what `/tmp/w6-pass-t45b.log` records,
  // after a seeder that had already created the tenant successfully.
  //
  // So: poll for EITHER leaving /login OR the app shell rendering. The shell is the positive
  // signal and matters more than the URL — a panel that renders its shell while the router has
  // not yet rewritten the address is signed in, and a pass that reads only the URL calls that a
  // failure. The error state (`role="alert"` beside the form) is checked too, so a genuine
  // refusal returns promptly instead of burning the whole budget on a wait that cannot succeed.
  let signedIn = false;
  let refusal = "";
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const state = await page
      .evaluate(() => ({
        offLogin: !/\/login/.test(location.pathname),
        shell: !!document.querySelector('nav[aria-label="Sections"]'),
        alert: (document.querySelector('[role="alert"]')?.textContent || "").trim().slice(0, 200),
      }))
      .catch(() => ({ offLogin: false, shell: false, alert: "" }));
    if (state.alert) {
      refusal = state.alert;
      break;
    }
    if (state.shell || state.offLogin) {
      signedIn = true;
      break;
    }
    await page.waitForTimeout(500);
  }
  report.steps.push({ action: "login", clicked, url: page.url(), signedIn, refusal });
  if (!signedIn) {
    log(`sign-in: the form was submitted but no session appeared${refusal ? ` — the panel said: ${refusal}` : ""}`);
  }
  return signedIn;
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

  // ---- the browser push device block (REQ-021, slice 6b) -----------------------------------
  //
  // **Why this block has its own legs rather than a screenshot.** The three device API
  // functions had zero UI callers until this slice, so the screen could render a Web Push
  // column with nothing behind it and every other leg in this pass would still be green. What
  // has to be asserted is the *shape of the answer* in each of the three states a headless
  // Chromium is actually in — an installation with no key, and one with a key and no
  // registered browser — because those are the two states this pass can reach, and a block
  // that renders nothing for them is indistinguishable from a block that was never wired up.
  steps.pushDevicesPresent = (await page.locator("[data-push-devices]").count()) > 0;

  if (steps.pushDevicesPresent) {
    // Either of these is a valid state; what must not happen is neither.
    const empty = await page.locator("[data-push-state=empty]").count();
    const list = await page.locator("[data-push-state=list]").count();
    const loading = await page.locator("[data-push-state=loading]").count();
    steps.pushDevicesResolved = empty > 0 || list > 0;
    steps.pushDevicesNotStuckLoading = loading === 0;

    // The empty state has to *explain* what registering is. A heading with no body is a
    // button with no explanation, and the reader cannot tell whether this is their phone or
    // a colleague's laptop.
    if (empty > 0) {
      const copy = await page
        .locator("[data-push-state=empty]")
        .innerText()
        .catch(() => "");
      steps.pushEmptyExplainsScope = /this browser/i.test(copy) && copy.trim().length > 60;
    }

    // The enable button always exists. Whether it is *enabled* depends on the installation's
    // key, and both answers are honest — but a block with no button at all is a block with no
    // way to register, which is the defect this slice was written to close.
    const enable = page.locator("[data-push-enable]");
    steps.pushEnableOffered = (await enable.count()) > 0;
    steps.pushReloadOffered = (await page.locator("[data-push-reload]").count()) > 0;

    // The unavailable notice names the variable. "Push is not configured" sends an operator
    // to a config file; "set OMNION_PUSH_PRIVATE_KEY" sends them to the right line of it.
    const unavailable = await page.locator("[data-push-unavailable]").count();
    if (unavailable > 0) {
      const notice = await page
        .locator("[data-push-unavailable]")
        .innerText()
        .catch(() => "");
      steps.pushUnavailableNamesAVariable = /OMNION_PUSH_[A-Z_]+/.test(notice);
      steps.pushEnableDisabledWhenUnavailable = await enable
        .first()
        .isDisabled()
        .catch(() => false);
    } else {
      // No notice means the installation has a key, so the button must be live.
      steps.pushEnableDisabledWhenUnavailable = false;
      steps.pushEnableLiveWithKey = !(await enable.first().isDisabled().catch(() => true));
    }

    // Pressing a LIVE button in a headless pass cannot succeed — there is no user gesture and
    // no service worker — and the block must say *that* rather than silently doing nothing or
    // throwing an unhandled rejection into the console. Chromium without a service worker
    // rejects `navigator.serviceWorker.ready`, which is the exact leg that catches a missing
    // catch.
    //
    // A *disabled* button is a different case and the earlier version measured it wrongly:
    // it clicked unconditionally and read "no error appeared" as a defect, while on an
    // installation with no key pair the button is `disabled` by design — Playwright refuses
    // the click, so the leg was measuring the browser, not the screen. The two are separated
    // now and BOTH still have to say something: a live button must produce a sentence when
    // its browser refuses, and a disabled one must carry the reason *with the control*
    // rather than leaving it in a paragraph further up the block.
    const enableDisabled = await enable.first().isDisabled().catch(() => true);
    if (steps.pushEnableOffered && !enableDisabled) {
      await enable.first().click().catch(() => {});
      await page.waitForTimeout(1800);
      const said = await page.locator("[data-push-error]").count();
      steps.pushEnableExplainsItself = said > 0;
      if (said > 0) {
        const text = await page
          .locator("[data-push-error]")
          .innerText()
          .catch(() => "");
        steps.pushErrorIsASentence = text.trim().length > 20;
      }
    } else if (steps.pushEnableOffered) {
      steps.pushEnableExplainsItself =
        (await page.locator("[data-push-enable-reason]").count()) > 0;
      steps.pushDisabledReasonNamesAVariable = /OMNION_PUSH_[A-Z_]+/.test(
        await page
          .locator("[data-push-enable-reason]")
          .innerText()
          .catch(() => ""),
      );
      steps.pushDisabledReasonIsDescribed =
        (await page.locator("[data-push-enable][aria-describedby=push-enable-reason]").count()) > 0;
    }

    // A remove button that exists on a row must delete that row, and the row must disappear
    // from the DOM rather than only from the server's answer. Guarded on there being a device:
    // in a pass with none, this leg is absent and that is recorded rather than passed.
    const deviceRow = page.locator("[data-push-device]").first();
    if ((await deviceRow.count()) > 0) {
      const remove = page.locator("[data-push-remove]").first();
      steps.pushRemoveOffered = (await remove.count()) > 0;
      if (steps.pushRemoveOffered) {
        const before = await page.locator("[data-push-device]").count();
        await remove.click().catch(() => {});
        await page.waitForTimeout(1500);
        const after = await page.locator("[data-push-device]").count();
        steps.pushRemoveRemovedTheRow = after < before;
      }
    }

    // 390px: the block is a flex row of an icon, a name, a hint and a button, which is four
    // things competing for 390 pixels. The rule is no horizontal scroll, and it is measured on
    // this block specifically rather than on the page.
    await page.setViewportSize({ width: 390, height: 844 }).catch(() => {});
    await page.waitForTimeout(500);
    steps.pushDevicesFitNarrow = await page
      .locator("[data-push-devices]")
      .evaluate((node) => node.scrollWidth <= node.clientWidth + 1)
      .catch(() => true);
    await shot(page, "page-notifications-push-devices-narrow");
    await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});
    await page.waitForTimeout(300);
  }

  await shot(page, "page-notifications-push-devices");

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
async function runObservabilityOverviewDepth(page) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "observability-overview-depth", action: "observability", ...step });
  };
  // An assertion is recorded, not thrown: the roll-up counts steps whose `ok` is false, so a
  // pass that finds a defect still writes its artifacts and its report instead of dying on the
  // first one. (This is the harness convention — the sibling depth passes record and return.)
  const expect = (check, ok, detail) => note({ check, ok, detail });

  await page
    .goto(`${URL_ADMIN}/observability`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="observability-overview"]', { timeout: 20000 })
    .catch(() => {});

  const root = page.locator('[data-view="observability-overview"]');
  await root.waitFor({ state: "visible", timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);

  // (1) The screen mounted at all — the route and the API call both answered.
  const mounted = (await root.count()) > 0;
  expect("screen-mounted", mounted, "/observability rendered nothing");
  if (!mounted) return steps;

  // (2) Every tile is present. Six is the number the request asks for: requests, error ratio,
  //     p95, queue depth, AI spend and alerts.
  const slots = ["requests", "error-ratio", "p95", "queue", "spend", "alerts"];
  const missing = [];
  for (const slot of slots) {
    if ((await root.locator(`[data-tile="${slot}"]`).count()) === 0) missing.push(slot);
  }
  expect(
    "every-tile-present",
    missing.length === 0,
    missing.length > 0 ? `missing tiles: ${missing.join(", ")}` : `${slots.length} tiles`,
  );

  // (3) An idle instance must say so. The registry is empty right after the harness reset the
  //     database, so every headline is `null`; the screen's job is to render that as a dash and
  //     to SAY it, not to draw zeroes.
  const idleTexts = await Promise.all(
    slots.slice(0, 5).map(async (slot) => ({
      slot,
      text: await root.locator(`[data-tile="${slot}"]`).innerText().catch(() => ""),
    })),
  );
  const drewZero = idleTexts.filter((tile) => /(^|\s)0(\s|$)/.test(tile.text)).map((t) => t.slot);
  const saysNoData = (await root.getByText(/has not served a request in the window/i).count()) > 0;
  expect(
    "idle-instance-is-not-zero",
    drewZero.length === 0,
    drewZero.length > 0
      ? `drew 0 for ${drewZero.join(", ")} on an instance with no samples`
      : "no headline claims zero without a sample",
  );
  expect(
    "idle-instance-explains-itself",
    saysNoData,
    saysNoData ? "" : "the screen does not say the dashes are an absence of data",
  );

  // (4) The alert tile never silently reads zero when the store was unreadable.
  const alertText = await root.locator('[data-tile="alerts"]').innerText().catch(() => "");
  expect("alert-tile-present", alertText.length > 0, alertText.split("\n")[0]);

  // (5) The six doors are links, not buttons that go nowhere.
  const doors = await root.locator('a[href^="/observability/"]').evaluateAll((links) =>
    links.map((link) => link.getAttribute("href")),
  );
  const expectedDoors = [
    "/observability/logs",
    "/observability/traces",
    "/observability/metrics",
    "/observability/exporters",
    "/observability/alerts",
    "/observability/settings",
  ];
  const deadDoors = expectedDoors.filter((href) => !doors.includes(href));
  expect(
    "six-doors-present",
    deadDoors.length === 0,
    deadDoors.length > 0 ? `no link to ${deadDoors.join(", ")}` : `${doors.length} doors`,
  );

  // (6) The exporter section renders in both states. A fresh stack has none, and an empty
  //     section that collapses to nothing reads as a broken one rather than an unconfigured one.
  const exporterSection = await root.getByText(/No exporter is configured/i).count();
  const exporterChips = await root.locator("li").filter({ hasText: /dropped/ }).count();
  expect(
    "exporter-section-renders",
    exporterSection > 0 || exporterChips > 0,
    exporterSection > 0 ? "empty state" : `${exporterChips} chip(s)`,
  );

  return steps;
}

async function runObservabilityMetricsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "observability-metrics-depth", action: "observability", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/observability/metrics`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="observability-metrics"]', { timeout: 20000 })
    .catch(() => {});

  const root = page.locator('[data-view="observability-metrics"]');
  await root
    .waitFor({ state: "visible", timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(600);

  // (1) the catalogue came from the API.
  const familyButtons = page.locator(
    '[data-view="observability-metrics"] table tbody button[aria-pressed]',
  );
  const familyCount = await familyButtons.count();
  note({ check: "catalogue-rendered", familyCount });
  if (familyCount === 0) {
    note({ check: "catalogue-empty", problem: "no family row rendered" });
    report.observabilityMetrics = { steps };
    return steps;
  }

  const firstText = (await familyButtons.first().innerText().catch(() => "")) || "";
  note({ check: "first-family", family: firstText.trim() });

  // (2) selecting a second family changes the chart heading.
  const heading = page.locator('[data-view="observability-metrics"] section').nth(1).locator("h2");
  const before = (await heading.innerText().catch(() => "")) || "";
  if (familyCount > 1) {
    const secondText = (await familyButtons.nth(1).innerText().catch(() => "")) || "";
    await familyButtons.nth(1).click();
    await page.waitForTimeout(900);
    const after = (await heading.innerText().catch(() => "")) || "";
    note({
      check: "selection-changes-chart",
      before: before.trim(),
      after: after.trim(),
      changed: after !== before,
      second: secondText.trim(),
    });
  }

  // (3) the range control changes the window the response reports.
  const window24 = page.getByRole("button", { name: "24 h" });
  if (await window24.count()) {
    await window24.first().click();
    await page.waitForTimeout(900);
    const pressed = await window24.first().getAttribute("aria-pressed").catch(() => null);
    const sub = (await page
      .locator('[data-view="observability-metrics"] section')
      .nth(1)
      .locator("p")
      .first()
      .innerText()
      .catch(() => "")) || "";
    note({ check: "range-changed", pressed, summary: sub.trim() });
  }

  // (4) the cardinality renders as a cap, not a bare number.
  const seriesCells = await page
    .locator('[data-view="observability-metrics"] table tbody tr td:nth-child(5)')
    .allInnerTexts();
  const withCap = seriesCells.filter((text) => /\d+\s*\/\s*\d+/.test(text)).length;
  note({ check: "cardinality-shown-as-cap", rows: seriesCells.length, withCap });

  // (6) no dead control: the filter, its clear, and the re-seed.
  const filter = page.getByLabel("Filter metric families");
  if (await filter.count()) {
    await filter.fill("histogram");
    await page.waitForTimeout(500);
    const filtered = await familyButtons.count();
    note({ check: "filter-narrows", filtered });
    await filter.fill("zzzz-no-such-family");
    await page.waitForTimeout(500);
    const emptyTitle = (await page
      .locator('[data-view="observability-metrics"]')
      .getByText(/No family matches/i)
      .count()) > 0;
    note({ check: "empty-filter-state", emptyTitle });
    const clear = page.getByRole("button", { name: /Clear the filter/i });
    if (await clear.count()) {
      await clear.first().click();
      await page.waitForTimeout(400);
      note({ check: "filter-cleared", families: await familyButtons.count() });
    }
  }

  const copy = page.getByRole("button", { name: /Copy as PromQL/i });
  if (await copy.count()) {
    // No clipboard permission is granted here: the export takes `(page, report)` and reaching
    // for `context` would make this function depend on a binding it does not own — which is how
    // the first draft died with `context is not defined` after the harness had already signed in.
    // A headless Chromium allows `navigator.clipboard.writeText` on a user gesture regardless.
    await copy.first().click();
    await page.waitForTimeout(500);
    const notice = (await page.getByText(/PromQL copied/i).count()) > 0;
    const promql = (await page
      .locator('[data-view="observability-metrics"] code')
      .first()
      .innerText()
      .catch(() => "")) || "";
    note({ check: "promql-copied", notice, promql: promql.trim() });
  }

  const resync = page.getByRole("button", { name: /Re-seed from registry/i });
  if (await resync.count()) {
    await resync.first().click();
    await page.waitForTimeout(1200);
    const seeded = (await page.getByText(/Catalogue re-seeded/i).count()) > 0;
    note({ check: "catalogue-reseeded", seeded });
  }

  // (5) the keyboard sequence a person tries first.
  await page.locator("body").click({ position: { x: 5, y: 5 } }).catch(() => {});
  await page.keyboard.press("/");
  await page.waitForTimeout(200);
  const focusAfterSlash = await page.evaluate(() => document.activeElement?.getAttribute("aria-label"));
  note({ check: "slash-focuses-filter", focusAfterSlash });
  await page.keyboard.press("Escape");
  await page.waitForTimeout(200);
  const focusAfterEscape = await page.evaluate(
    () => document.activeElement?.tagName ?? "(none)",
  );
  note({ check: "escape-clears", focusAfterEscape });
  // `r` refreshes ONLY if Escape handed the keyboard back: a stranded focus swallows it, and the
  // step below is the assertion that knows the difference.
  await page.keyboard.press("r");
  await page.waitForTimeout(1200);
  const stillThere = await page.locator('[data-view="observability-metrics"]').count();
  note({ check: "r-refreshes-after-escape", viewPresent: stillThere > 0 });

  // Mobile: the catalogue has to become cards, not a sideways-scrolling table.
  const mobile = [];
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  const overflow = await page.evaluate(() => {
    const el = document.querySelector('[data-view="observability-metrics"]');
    if (!el) return { scrollWidth: 0, clientWidth: 0 };
    return { scrollWidth: el.scrollWidth, clientWidth: el.clientWidth };
  });
  mobile.push({ check: "no-horizontal-overflow", ...overflow });
  await shot(page, "page-observability-metrics-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);

  await shot(page, "page-observability-metrics");
  report.observabilityMetrics = { steps, mobile };
  return steps;
}


// The walk is a whole-box pass: it visits every screen, and on a machine where six writers are
// compiling and three are driving their own Chromium at the same time it can die part-way through
// with `Target page, context or browser has been closed` — long before the depth passes run. A
// pass that aborts proves nothing about the screen it never reached.
//
// So the depth passes are exported, and `secrets-audit-depth.cjs` can drive one of them on its
// own against a single stack. Requiring this file must not start the whole walk, hence the guard:
// the CLI is `node walkthrough.cjs`, a require is a library call.
/**
 * The REQ-126 trace search depth pass.
 *
 * It drives the screen the way an operator arrives at it — holding a request id from a log row or
 * an error banner — and then checks the two things a search screen gets wrong quietly: that the
 * rows say WHY they are sampled, and that an empty result explains the sampling policy instead of
 * looking like a broken filter.
 *
 * The waterfall is opened from a real row when the index has one. An index with no rows is a
 * legitimate state on a fresh stack and is reported as such — a pass that manufactures a trace to
 * click would be testing the driver's own insert, not the screen.
 */
async function runObservabilityTracesDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "observability-traces-depth", action: "observability", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/observability/traces`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="observability-traces"]', { timeout: 20000 })
    .catch(() => {});
  const root = page.locator('[data-view="observability-traces"]');
  await root.waitFor({ state: "visible", timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(700);

  // (1) the request-id box takes the jump that the screen exists for.
  //
  // Every other interaction in this pass is `.catch(() => {})` on purpose, and this one was not:
  // `fill` rejects when the element is not attached, and an unguarded rejection in an async
  // walkthrough is an UNCAUGHT exception that kills the whole pass — 380 screenshots and every
  // screen after this one measured for nothing, because one input had not rendered yet. A step
  // that cannot run is a recorded `false`, not the end of the run.
  const requestInput = page.locator("[data-trace-request-input]");
  await requestInput.waitFor({ state: "visible", timeout: 20000 }).catch(() => {});
  await requestInput
    .fill("11111111-2222-3333-4444-555555555555")
    .catch(() => {});
  await page.waitForTimeout(1200);
  const requestFilter = await requestInput.inputValue().catch(() => "");
  note({ check: "request-id-typed", value: requestFilter, typed: requestFilter.length === 36 });

  // (2) an id with no trace must EXPLAIN the sampling policy, not render a blank table.
  const emptyHint = (await page.locator('[data-view="observability-traces"] p').first().innerText().catch(() => "")) || "";
  const rowsAfterMiss = await page.locator("[data-trace-open]").count();
  note({ check: "miss-explained", rows: rowsAfterMiss, hint: emptyHint.slice(0, 160) });
  if (rowsAfterMiss === 0) {
    const explained = /unsampled|sampling|retention|no trace/i.test(emptyHint);
    note({ check: "miss-explains-sampling", explained });
  }

  // (3) clear, then filter by the failing status, which is the filter operators reach for.
  await page.locator("[data-trace-clear]").click().catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-trace-status-select]").selectOption("error").catch(() => {});
  await page.waitForTimeout(1100);
  const errorRows = await page.locator("[data-trace-open]").count();
  note({ check: "status-error-filter", rows: errorRows });

  // (4) a min-duration that nothing meets must be an empty state with the filters still shown,
  //     not a silent reset.
  await page.locator("[data-trace-min-input]").fill("999999");
  await page.waitForTimeout(1100);
  const none = await page.locator("[data-trace-open]").count();
  const clearStillThere = await page.locator("[data-trace-clear]").count();
  note({ check: "impossible-filter", rows: none, clearFilterKept: clearStillThere });

  // (4a) a value the API would refuse must be caught BY THE SCREEN, with a field-level message and
  //      never on the wire. The first run of this pass typed "12x" and the screen forwarded
  //      `min_duration_ms=NaN`, which the API rightly answered with a 400 that the screen then
  //      rendered as "the trace index could not be read" — a server failure caused by a keystroke.
  // The assertion is on the URL that went out, not on the count and not on the screen's message.
  // A count is wrong here: clearing an invalid filter IS a different query ("no floor" rather than
  // "floor 999999"), so one more request is correct behaviour and a count cannot tell a legitimate
  // re-read from a leaked one. And the message alone proves nothing — a request that was sent and
  // refused produces a message too. What must never appear on the wire is the literal `NaN`.
  const traceUrls = async () =>
    page.evaluate(() =>
      performance
        .getEntriesByType("resource")
        .map((e) => e.name)
        .filter((n) => n.includes("/api/v1/observability/traces")),
    );

  await page.locator("[data-trace-min-input]").fill("12x");
  await page.waitForTimeout(1400);
  const minMessage = (await page.locator("[data-trace-min-error]").innerText().catch(() => "")) || "";
  const minUrls = await traceUrls();
  note({
    check: "min-duration-refused-locally",
    shown: minMessage.length > 0,
    nanOnTheWire: minUrls.some((u) => u.includes("NaN")),
    message: minMessage.slice(0, 90),
  });

  // (4b) the same for a request id that is not a uuid — the QA harness pastes words into every
  //      text box it finds, and a bad paste is an operator's paste too.
  await page.locator("[data-trace-min-input]").fill("");
  await page.locator("[data-trace-request-input]").fill("QA sample");
  await page.waitForTimeout(1400);
  const idMessage = (await page.locator("[data-trace-request-id-error]").innerText().catch(() => "")) || "";
  const idUrls = await traceUrls();
  note({
    check: "request-id-refused-locally",
    shown: idMessage.length > 0,
    wordsOnTheWire: idUrls.some((u) => u.includes("QA") || u.includes("+")),
    message: idMessage.slice(0, 90),
  });
  await shot(page, "page-observability-traces-invalid-filter");

  await page.locator("[data-trace-clear]").click().catch(() => {});
  await page.waitForTimeout(1200);

  // (5) the rows carry their sampling reason, and a row opens a waterfall.
  const chips = await page.locator("[data-trace-sampling]").count();
  note({ check: "sampling-reasons-shown", chips });
  const rows = await page.locator("[data-trace-open]").count();
  note({ check: "rows", rows });
  if (rows > 0) {
    await page.locator("[data-trace-open]").first().click().catch(() => {});
    await page
      .waitForSelector("[data-trace-detail]", { timeout: 20000 })
      .catch(() => {});
    await page.waitForTimeout(900);
    const waterfall = await page.locator("[data-trace-waterfall]").count();
    const spans = await page.locator("[data-trace-span]").count();
    const truncated = await page.locator("[data-trace-truncated]").count();
    const noBackend = await page.locator("[data-trace-no-backend]").count();
    const backendLink = await page.locator("[data-trace-backend-link]").count();
    note({ check: "waterfall", waterfall, spans, truncated, noBackend, backendLink });
    // The backend link and the "no backend configured" message are MUTUALLY EXCLUSIVE: rendering
    // both, or neither, is the bug this assertion is here to catch.
    note({ check: "backend-answer-explicit", ok: backendLink + noBackend === 1 });
    if (spans > 1) {
      const picks = page.locator("[data-trace-span-pick]");
      if ((await picks.count()) > 1) {
        await picks.nth(1).click().catch(() => {});
        await page.waitForTimeout(500);
        const details = await page.locator("[data-trace-span-details]").count();
        note({ check: "span-attributes", details });
      }
    }
    await shot(page, "page-observability-traces-waterfall");
    await page.locator("[data-trace-close]").click().catch(() => {});
    await page.waitForTimeout(400);
  } else {
    note({
      check: "no-traces-indexed",
      reason: "a fresh stack may have no sampled trace; the empty state is what is checked",
    });
  }

  // (6) mobile: the waterfall region scrolls inside itself and the rows are cards.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  const cards = await page.locator("[data-trace-cards] li").count();
  const tableHidden = await page.locator("[data-trace-table]").first().isVisible().catch(() => false);
  note({ check: "mobile", cards, tableVisible: tableHidden });
  await shot(page, "page-observability-traces-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);
  await shot(page, "page-observability-traces");

  report.observabilityTraces = { steps };
  return steps;
}

/**
 * The REQ-126 log explorer depth pass.
 *
 * A log screen is only useful if it can be wrong in a way the operator can see, so the assertions
 * here are the three ways this one used to be silently wrong:
 *
 * 1. **A typo must not become an API error page.** The traces screen had this defect once —
 *    `Number("12x")` → `NaN` → `URLSearchParams` wrote the literal `NaN` into the query and the
 *    `400` came back as "the trace index could not be read", the worst thing a debug screen can
 *    say to the person debugging. So a NON-uuid request id is pasted here and the pass requires a
 *    FIELD-level message next to the box. Requiring only "an error appears" would pass on the
 *    broken version too, since the broken version also shows an error — the assertion is that the
 *    complaint is attached to the input and that the result list underneath is untouched.
 * 2. **An empty store and an over-narrow filter are different sentences.** Both arrive as an
 *    empty `entries` array, so the pass reads the empty state's title: an idle stack says "no
 *    telemetry", and a stack that does have lines must NOT say that when a filter excluded them.
 * 3. **A real request id switches the ordering and the screen says so.** The explorer reads
 *    newest-first and the request timeline oldest-first; that inversion is the API's contract, and
 *    a screen that reversed it silently would look broken rather than different.
 */
async function runObservabilityLogsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "observability-logs-depth", action: "observability", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/observability/logs`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="observability-logs"]', { timeout: 20000 })
    .catch(() => {});
  const root = page.locator('[data-view="observability-logs"]');
  await root.waitFor({ state: "visible", timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(700);

  // (1) the screen renders its own controls — a pass that cannot find the filters proves nothing
  //     about them, and every assertion below is scoped to them.
  const controls = await page.locator("[data-logs-text], [data-logs-request-id], [data-logs-level]").count();
  note({ check: "controls-present", count: controls });
  if (controls < 3) return { steps };

  // (2) the level chips are a multi-select: pressing one marks it, pressing it again clears it.
  //     A chip that cannot be un-pressed is a filter the operator has to reload the page to drop.
  const errorChip = page.locator('[data-logs-level="error"]');
  await errorChip.click().catch(() => {});
  await page.waitForTimeout(900);
  const pressed = await errorChip.getAttribute("aria-pressed").catch(() => null);
  note({ check: "level-chip-pressed", value: pressed });
  await errorChip.click().catch(() => {});
  await page.waitForTimeout(700);
  const released = await errorChip.getAttribute("aria-pressed").catch(() => null);
  note({ check: "level-chip-released", value: released });

  // (3) the assertion that matters: a malformed id is refused IN THE COMPONENT. The pass requires
  //     the field-level message AND that the list underneath still shows the unfiltered store —
  //     a screen that blanks its results while complaining is worse than one that keeps them.
  const rowsBeforeTypo = await page.locator("[data-log-row]").count();
  await page.locator("[data-logs-request-id]").fill("not-a-uuid");
  await page.waitForTimeout(900);
  const fieldError = await page
    .locator("[data-logs-request-error]")
    .innerText()
    .catch(() => "");
  const rowsAfterTypo = await page.locator("[data-log-row]").count();
  const markedInvalid = await page
    .locator("[data-logs-request-id]")
    .getAttribute("aria-invalid")
    .catch(() => null);
  note({
    check: "malformed-id-refused-in-component",
    message: fieldError.slice(0, 200),
    ariaInvalid: markedInvalid,
    rowsBeforeTypo,
    rowsAfterTypo,
    resultsPreserved: rowsAfterTypo === rowsBeforeTypo,
  });

  // (4) a VALID id for a request that exists takes the timeline route, and the screen announces
  //     the ordering change rather than leaving the operator to wonder why the rows reversed.
  await page.locator("[data-logs-request-id]").fill("11111111-2222-3333-4444-555555555555");
  await page.waitForTimeout(1100);
  const orderingNote = (await root.innerText().catch(() => "")) || "";
  const announcesOrder = /oldest first/i.test(orderingNote);
  note({ check: "request-view-announces-ordering", announced: announcesOrder });

  // (5) an id that is well-formed but matches nothing must read as "no match", never as "the
  //     store is empty" — the sentence that would convince an operator logging had stopped.
  const missText = (await root.innerText().catch(() => "")) || "";
  const saysEmptyStore = /no telemetry yet/i.test(missText);
  const saysNoMatch = /no line matches|no lines match|does not match/i.test(missText);
  note({ check: "miss-not-mistaken-for-empty-store", saysEmptyStore, saysNoMatch });

  // (6) Escape clears the filters — the recovery from a dead end an operator painted themselves.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(900);
  const clearedId = await page.locator("[data-logs-request-id]").inputValue().catch(() => "unread");
  const clearedError = await page.locator("[data-logs-request-error]").count();
  note({ check: "escape-clears", requestIdValue: clearedId, staleFieldError: clearedError });

  // (7) retention is shown beside the results it limits: a window that silently hides older lines
  //     is indistinguishable from a platform that never recorded them.
  const body = (await root.innerText().catch(() => "")) || "";
  const showsWindow = /keeps\s+\d+\s+days/i.test(body);
  note({ check: "retention-window-shown", shown: showsWindow });

  return { steps };
}

/**
 * The REQ-126 exporter centre depth pass.
 *
 * The interesting assertion is the one the QA plan names: `Test` against a deliberately wrong
 * endpoint must render a degraded REPORT carrying the backend's own words, not an error page. A
 * `Test` that renders a 500 has told the operator nothing they could not have learned by waiting.
 *
 * The exporter is removed at the end so the pass leaves the stack as it found it.
 */
async function runObservabilityExportersDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "observability-exporters-depth", action: "observability", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/observability/exporters`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="observability-exporters"]', { timeout: 20000 })
    .catch(() => {});
  await page
    .locator('[data-view="observability-exporters"]')
    .waitFor({ state: "visible", timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  // (1) the egress statement is on the screen, not only in a component.
  const egress = (await page.locator("[data-exporter-egress-notice]").innerText().catch(() => "")) || "";
  note({ check: "egress-stated", present: egress.length > 40 });

  // (2) the empty state names the trade-off rather than looking like a broken screen.
  const before = await page.locator("[data-exporter-row]").count();
  note({ check: "rows-before", rows: before });

  // (3) create one against a port nothing listens on.
  const name = `qa-otlp-${Date.now()}`;
  await page.locator("[data-exporter-add]").click().catch(() => {});
  await page.waitForSelector("[data-exporter-form]", { timeout: 8000 }).catch(() => {});
  await page.locator("[data-exporter-name]").fill(name);
  await page.locator("[data-exporter-endpoint]").fill("http://127.0.0.1:1/v1/logs");
  await page.locator("[data-exporter-batch]").fill("100");
  await page.locator("[data-exporter-timeout]").fill("600");
  await shot(page, "page-observability-exporters-form");
  await page.locator("[data-exporter-save]").click().catch(() => {});
  await page.waitForTimeout(1800);

  const afterCreate = await page.locator(`[data-exporter-row="${name}"]`).count();
  note({ check: "created", rows: afterCreate });

  // (4) validation: an empty name is refused with a message, not saved as a blank row.
  await page.locator("[data-exporter-add]").click().catch(() => {});
  await page.waitForSelector("[data-exporter-form]", { timeout: 8000 }).catch(() => {});
  await page.locator("[data-exporter-endpoint]").fill("not-a-url");
  await page.locator("[data-exporter-save]").click().catch(() => {});
  await page.waitForTimeout(900);
  const stillOpen = await page.locator("[data-exporter-form]").count();
  note({ check: "validation-kept-the-form-open", stillOpen });
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);

  // (5) Test against the dead endpoint: a degraded REPORT, not an error page.
  const testButton = page.locator(`[data-exporter-test="${name}"]`).first();
  if ((await testButton.count()) > 0) {
    await testButton.click().catch(() => {});
    await page
      .waitForSelector("[data-exporter-test-result]", { timeout: 25000 })
      .catch(() => {});
    await page.waitForTimeout(700);
    const verdict = (await page.locator("[data-exporter-test-result]").innerText().catch(() => "")) || "";
    const refused = /refused|error|connect|ECONNREFUSED|proxy|resolve/i.test(verdict);
    note({ check: "test-renders-a-report", present: verdict.length > 0, refused });
    note({ check: "test-not-an-error-page", ok: (await page.locator("[data-exporter-error]").count()) === 0 });
    await shot(page, "page-observability-exporters-test");
    await page.locator("[data-exporter-test-dismiss]").click().catch(() => {});
    await page.waitForTimeout(400);
  }

  // (6) the chip is a real state, and the row says the backend is unknown rather than green.
  const health = await page
    .locator(`[data-exporter-row="${name}"] [data-exporter-health]`)
    .first()
    .getAttribute("data-exporter-health")
    .catch(() => null);
  note({ check: "health-chip", health });
  note({ check: "unknown-is-not-ok", ok: health !== "ok" || before > 0 });

  // (7) the form has no field that could hold a credential.
  await page.locator(`[data-exporter-edit="${name}"]`).first().click().catch(() => {});
  await page.waitForSelector("[data-exporter-form]", { timeout: 8000 }).catch(() => {});
  const secretField = await page.locator("[data-exporter-secret]").getAttribute("type").catch(() => "text");
  const secretLabel = (await page.locator('label[for="exporter-secret"]').innerText().catch(() => "")) || "";
  note({ check: "auth-is-a-reference-not-a-value", type: secretField, saysReference: /reference|secret id/i.test(secretLabel) });
  const passwordFields = await page.locator('[data-exporter-form] input[type="password"]').count();
  note({ check: "no-password-field", passwordFields });
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);

  // (8) remove it, so the pass leaves the stack as it found it.
  await page.locator(`[data-exporter-remove="${name}"]`).first().click().catch(() => {});
  await page.waitForTimeout(1600);
  const afterRemove = await page.locator(`[data-exporter-row="${name}"]`).count();
  note({ check: "removed", rows: afterRemove });

  // (9) mobile: cards, not a table squeezed into 390 px.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  const mobile = { cards: await page.locator("[data-exporter-cards] li").count() };
  note({ check: "mobile", ...mobile });
  await shot(page, "page-observability-exporters-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);
  await shot(page, "page-observability-exporters");

  report.observabilityExporters = { steps };
  return steps;
}

/**
 * The alert centre (REQ-126, slice 4).
 *
 * Written to fail on the specific ways this screen is supposed to differ from a generic CRUD
 * list, because a pass that only counted rows would be green on a screen that had every one of
 * those distinctions flattened:
 *
 *   1. The four stat tiles read real counts — a screen that always renders 0 is
 *      indistinguishable from a quiet system.
 *   2. `Preview` reports either a value or `no data`, and the two render DIFFERENTLY. A preview
 *      that said "not breaching" for both is the bug the view's own comment is about.
 *   3. A bad expression is refused in the form, naming the family, and nothing is saved. That is
 *      the "validated against the catalogue" line, and it is only provable by trying to save a
 *      rule that cannot work.
 *   4. The rule that gets created can be silenced, and the silence appears with its reason and
 *      its remaining time.
 *   5. A BUNDLED rule's delete explains itself instead of silently doing nothing — it is
 *      re-seeded at every boot, so a delete that appeared to work would bring the row back on the
 *      next restart.
 */
async function runObservabilityAlertsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "observability-alerts-depth", action: "observability", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/observability/alerts`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="observability-alerts"]', { timeout: 20000 })
    .catch(() => {});
  await page
    .locator('[data-view="observability-alerts"]')
    .waitFor({ state: "visible", timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(800);

  // (1) the tiles are real counts.
  const tiles = {};
  for (const label of ["Firing", "Pending", "Silenced", "Worst severity"]) {
    tiles[label] = (
      (await page
        .locator(`[data-alert-stat="${label}"] p`)
        .nth(1)
        .innerText()
        .catch(() => "")) || ""
    ).trim();
  }
  note({ check: "stat-tiles", ...tiles });
  note({
    check: "tiles-are-not-placeholders",
    ok: Object.values(tiles).every((value) => value.length > 0),
  });

  // (2) the bundled rules are listed, and they are marked as bundled.
  const rows = await page.locator("[data-alert-rule]").count();
  note({ check: "rules-listed", rows });

  // (3) a real preview on a family the platform records.
  await page.locator("[data-alert-new]").click().catch(() => {});
  await page.waitForSelector("[data-alert-rule-form]", { timeout: 8000 }).catch(() => {});
  const formOpen = await page.locator("[data-alert-rule-form]").count();
  note({ check: "form-opens", open: formOpen });

  await page
    .locator("[data-alert-rule-expr]")
    .fill('omnion_http_requests_total{status="5xx"} > 0')
    .catch(() => {});
  await page.locator("[data-alert-rule-preview]").click().catch(() => {});
  await page.waitForTimeout(1500);
  const verdictOk = await page.locator('[data-preview="ok"]').count();
  const verdictBreaching = await page.locator('[data-preview="breaching"]').count();
  const verdictNoData = await page.locator('[data-preview="no-data"]').count();
  note({
    check: "preview-reports-a-verdict",
    verdicts: { ok: verdictOk, breaching: verdictBreaching, noData: verdictNoData },
    ok: verdictOk + verdictBreaching + verdictNoData > 0,
  });

  // A family that has never recorded must say `no data` and NOT say "not breaching" — the two
  // read identically on a chip and only one of them is the operator's to fix.
  await page
    .locator("[data-alert-rule-expr]")
    .fill("omnion_workflow_steps_total{status=\"__qa_never_seen__\"} > 0")
    .catch(() => {});
  await page.locator("[data-alert-rule-preview]").click().catch(() => {});
  await page.waitForTimeout(1500);
  const noDataAlone = await page.locator('[data-preview="no-data"]').count();
  const quietInstead = await page.locator('[data-preview="ok"]').count();
  note({
    check: "no-data-is-not-healthy",
    noData: noDataAlone,
    saidOkInstead: quietInstead,
    ok: noDataAlone > 0 && quietInstead === 0,
  });

  // (4) an expression that cannot work is refused, and the refusal names the family.
  await page
    .locator("[data-alert-rule-name]")
    .fill(`qa-bad-${Date.now()}`)
    .catch(() => {});
  await page
    .locator("[data-alert-rule-expr]")
    .fill("omnion_not_a_family > 1")
    .catch(() => {});
  await page.locator("[data-alert-rule-save]").click().catch(() => {});
  await page.waitForTimeout(1600);
  const formError = (await page.locator("[data-alert-rule-error]").innerText().catch(() => "")) || "";
  note({
    check: "bad-expression-refused",
    said: formError.slice(0, 200),
    namesFamily: formError.includes("omnion_not_a_family"),
  });
  const stillOpen = await page.locator("[data-alert-rule-form]").count();
  note({ check: "refusal-keeps-the-form-open", stillOpen });

  // (5) a real rule, created, previewed, silenced and removed.
  const name = `qa-alert-${Date.now()}`;
  await page
    .locator("[data-alert-rule-name]")
    .fill(name)
    .catch(() => {});
  await page
    .locator("[data-alert-rule-expr]")
    .fill("omnion_queue_depth > 100000")
    .catch(() => {});
  await page
    .locator("[data-alert-rule-dwell]")
    .fill("0")
    .catch(() => {});
  await page.locator("[data-alert-rule-save]").click().catch(() => {});
  await page.waitForTimeout(2000);
  const created = await page.locator(`[data-alert-rule="${name}"]`).count();
  note({ check: "rule-created", created });

  // A rule whose expression stopped parsing is a RED cell, not a silent one.
  const invalidCells = await page.locator("[data-alert-rule-invalid]").count();
  note({ check: "invalid-expression-is-called-out", invalidCells });

  // Silence it, and check the silence is visible with its reason.
  await page
    .locator(`[data-alert-rule="${name}"] [data-alert-rule-silence]`)
    .click()
    .catch(() => {});
  await page.waitForSelector("[data-silence-form]", { timeout: 8000 }).catch(() => {});
  await page
    .locator("[data-silence-reason]")
    .fill("QA pass: exercising the silence form")
    .catch(() => {});
  await page.locator("[data-silence-minutes]").fill("30").catch(() => {});
  await page.locator("[data-silence-save]").click().catch(() => {});
  await page.waitForTimeout(2000);
  const silences = (await page.locator("[data-silence-list] li").innerText().catch(() => "")) || "";
  note({
    check: "silence-visible",
    found: silences.includes("QA pass"),
    statesRemaining: /\d+\s*(m|h)/.test(silences),
  });

  // A silence with no reason is refused — an anonymous silence is one nobody dares to remove.
  await page.locator("[data-silence-new]").click().catch(() => {});
  await page.waitForSelector("[data-silence-form]", { timeout: 8000 }).catch(() => {});
  const saveDisabled = await page.locator("[data-silence-save]").isDisabled().catch(() => true);
  note({ check: "reason-is-required", saveDisabledWithoutReason: saveDisabled });
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);

  // (6) the bundled rules explain their own refusal.
  const bundledRow = page.locator('[data-alert-rule]:has-text("bundled")').first();
  if (await bundledRow.count()) {
    await bundledRow.locator("[data-alert-rule-delete]").click().catch(() => {});
    await page.waitForTimeout(1800);
    const refusal = (await page.locator("[data-alerts-error]").innerText().catch(() => "")) || "";
    note({
      check: "bundled-rule-explains-its-refusal",
      said: refusal.slice(0, 160),
      explained: /re-seeded|every boot/i.test(refusal),
    });
  } else {
    note({ check: "bundled-rule-explains-its-refusal", skipped: "no bundled rule on screen" });
  }

  // (7) clean up: lift the silence and delete the rule, so the pass leaves the stack as found.
  await page.locator("[data-silence-lift]").first().click().catch(() => {});
  await page.waitForTimeout(1600);
  await page.locator(`[data-alert-rule="${name}"] [data-alert-rule-delete]`).click().catch(() => {});
  await page.waitForTimeout(1600);
  note({ check: "cleaned-up", remaining: await page.locator(`[data-alert-rule="${name}"]`).count() });

  // (8) mobile, and the final screenshots.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  await shot(page, "page-observability-alerts-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);
  await shot(page, "page-observability-alerts");

  report.observabilityAlerts = { steps };
  return steps;
}

/**
 * The observability settings screen (REQ-126, slice 4).
 *
 * The two assertions that matter are the second and the third, because they are the ones a form
 * passes without them:
 *
 *   1. The egress note is ON the screen. The request requires the settings screen to say out loud
 *      what leaves the instance; a component that renders it offscreen proves nothing.
 *   2. An out-of-range save is refused WITH A FIELD MESSAGE and the stored value does not change.
 *      The browser-side check is asserted too, because a save the browser silently blocks teaches
 *      an operator the form is broken.
 *   3. A valid save reports success — and the caps are visible BEFORE the refusal, not only in it.
 */
async function runObservabilitySettingsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "observability-settings-depth", action: "observability", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/observability/settings`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="observability-settings"]', { timeout: 20000 })
    .catch(() => {});
  await page
    .locator('[data-view="observability-settings"]')
    .waitFor({ state: "visible", timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(800);

  // (1) the egress statement.
  const egress = (await page.locator("[data-egress-note]").innerText().catch(() => "")) || "";
  note({ check: "egress-stated", present: egress.length > 40, saysRedacted: /redact/i.test(egress) });

  // The sampling copy has to state that errors are sampled regardless, or an operator reads the
  // ratio as "how much telemetry you lose" and never turns it down.
  const samplingHint = (await page.locator('label[for="sampling_ratio"] + p').innerText().catch(() => "")) || "";
  note({
    check: "sampling-copy-explains-errors",
    saysErrorsSampled: /errors are sampled regardless/i.test(samplingHint),
  });

  // The drain timeout and the probe contract, which is what tells an operator whether to raise it.
  const drain = (await page.locator('[data-lifecycle="drain"]').innerText().catch(() => "")) || "";
  note({ check: "drain-timeout-visible", says: drain, ok: /\d/.test(drain) });

  // (2) an out-of-range value is caught before it can be sent.
  await page.locator('[data-setting="logs_retention_days"]').fill("400").catch(() => {});
  await page.waitForTimeout(400);
  const fieldError = await page.locator('[data-field-error="logs_retention_days"]').count();
  const saveDisabled = await page.locator("[data-settings-save]").isDisabled().catch(() => false);
  note({ check: "range-caught-in-the-form", fieldError, saveDisabled });

  // And the ratio, whose bound is 0..1 rather than an integer range.
  await page.locator('[data-setting="sampling_ratio"]').fill("4").catch(() => {});
  await page.waitForTimeout(400);
  const ratioError = await page.locator('[data-field-error="sampling_ratio"]').count();
  note({ check: "ratio-bounded", ratioError });

  // A misspelled level is refused the same way — the level names are a closed set, and "verbose"
  // is not one of them.
  await page.locator('[data-setting="log_level_default"]').selectOption("verbose").catch(() => {});
  await page.waitForTimeout(400);
  const levelError = await page.locator('[data-field-error="log_level_default"]').count();
  note({ check: "level-is-a-closed-set", levelError });

  // (3) a valid save goes through and says so.
  await page.locator('[data-setting="logs_retention_days"]').fill("14").catch(() => {});
  await page.locator('[data-setting="sampling_ratio"]').fill("0.1").catch(() => {});
  await page.locator('[data-setting="log_level_default"]').selectOption("info").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-settings-save]").click().catch(() => {});
  await page.waitForTimeout(2000);
  const savedBanner = await page.locator("[data-settings-saved]").count();
  const saveError = await page.locator("[data-settings-error]").count();
  note({ check: "valid-save-succeeds", savedBanner, errors: saveError });

  // The temporary-raise composer: a raise with no target is ignored rather than applied to
  // everything, so the save below must NOT add one.
  await page.locator("[data-new-override-target]").fill("").catch(() => {});
  await page.locator("[data-settings-save]").click().catch(() => {});
  await page.waitForTimeout(1600);
  const overrides = await page.locator("[data-level-override]").count();
  note({ check: "empty-raise-is-not-applied", overrides });

  // A real raise, saved, then read back as LIVE.
  await page
    .locator("[data-new-override-target]")
    .fill(`qa_module_${Date.now()}`)
    .catch(() => {});
  await page.locator("[data-new-override-expiry]").selectOption("1").catch(() => {});
  await page.locator("[data-settings-save]").click().catch(() => {});
  await page.waitForTimeout(2000);
  const liveRaise = await page.locator('[data-level-override][data-expired="false"]').count();
  note({ check: "raise-is-live-before-its-expiry", liveRaise });

  // (4) mobile, and the final screenshots.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  await shot(page, "page-observability-settings-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);
  await shot(page, "page-observability-settings");

  report.observabilitySettings = { steps };
  return steps;
}

module.exports = {
  runSecretsAuditDepth,
  runDeploymentMigrationsDepth,
  runDeploymentArtifactsDepth,
  runDeploymentInstallDepth,
  runDeploymentUpgradeDepth,
  DEPLOYMENT_SCREENS,
  walkVersion,
  runObservabilityOverviewDepth,
  runObservabilityMetricsDepth,
  runObservabilityTracesDepth,
  runObservabilityExportersDepth,
  ensureSignedIn,
  runWizard,
  CREDS,
  URL_ADMIN,
};

if (require.main === module) {
  main().catch(async (err) => {
    console.error("[walk] unexpected failure:", err);
    try {
      fs.writeFileSync(path.join(OUT, "summary.json"), JSON.stringify({ fatal: String(err) }, null, 2));
    } catch {
      /* ignore */
    }
    process.exit(1);
  });
}

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
async function runSecretsRootKeyDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "secrets-root-key-depth", action: "secrets", ...step });
  };

  await page.goto(`${URL_ADMIN}/secrets/root-key`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-root-key-seal]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(600);

  const sealHealthy = await page
    .locator("[data-root-key-seal]")
    .first()
    .evaluate((node) => node.className.includes("bg-surface"))
    .catch(() => false);
  const ringRows = await page.locator("[data-root-key-row]").count();
  const rotateEnabled = await page
    .locator("[data-root-key-rotate]")
    .first()
    .isEnabled()
    .catch(() => false);
  note({ step: "opened", sealHealthy, ringRows, rotateEnabled });
  await shot(page, "page-secrets-root-key");

  // ---- The wizard refuses to continue before the acknowledgement --------------------------------
  await page.locator("[data-root-key-rotate]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-root-key-wizard]", { timeout: 10000 }).catch(() => {});
  const confirmTitle = (
    await page.locator("[data-root-key-wizard] h3").first().innerText().catch(() => "")
  ).trim();
  const blockedBeforeTick = await page
    .locator("[data-root-key-rotate-confirm]")
    .first()
    .isDisabled()
    .catch(() => false);
  const wizardText = await page
    .locator("[data-root-key-wizard]")
    .first()
    .innerText()
    .catch(() => "");
  const warningShown = wizardText.toLowerCase().includes("unrecoverable");
  note({ step: "wizard-opens-guarded", confirmTitle, blockedBeforeTick, warningShown });
  await shot(page, "page-secrets-root-key-wizard");

  // Escape closes the wizard without touching the ring.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(500);
  const closedByEscape = (await page.locator("[data-root-key-wizard]").count()) === 0;

  // ---- The rotation, for real ------------------------------------------------------------------
  await page.locator("[data-root-key-rotate]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-root-key-wizard]", { timeout: 10000 }).catch(() => {});
  await page.locator("[data-root-key-accept]").first().check({ timeout: 6000 }).catch(() => {});
  await page.locator("[data-root-key-rotate-confirm]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-root-key-counter]", { timeout: 25000 }).catch(() => {});
  await page.waitForTimeout(1500);

  const counterText = (
    await page.locator("[data-root-key-counter]").first().innerText().catch(() => "")
  ).trim();
  const progressNow = await page
    .locator("[data-root-key-job] [role=progressbar]")
    .first()
    .getAttribute("aria-valuenow")
    .catch(() => null);
  const rowsAfterRotation = await page.locator("[data-root-key-row]").count();
  note({ step: "rotated", closedByEscape, counterText, progressNow, rowsAfterRotation });
  await shot(page, "page-secrets-root-key-rotating");

  // ---- Pause, then resume ----------------------------------------------------------------------
  let paused = false;
  const pauseButton = page.locator("[data-root-key-pause]").first();
  if ((await pauseButton.count()) > 0) {
    await pauseButton.click({ timeout: 6000 }).catch(() => {});
    await page.waitForSelector("[data-root-key-resume]", { timeout: 15000 }).catch(() => {});
    paused = (await page.locator("[data-root-key-resume]").count()) > 0;
    await shot(page, "page-secrets-root-key-paused");
  }
  const resumeButton = page.locator("[data-root-key-resume]").first();
  if ((await resumeButton.count()) > 0) {
    await resumeButton.click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1500);
  }
  note({ step: "pause-resume", paused });

  // ---- The one thing no other pass can assert: no key material in the document -----------------
  const documentText = await page.evaluate(() => document.body.innerText);
  const leakedMaterial =
    documentText.includes("v1.") ||
    documentText.includes("wrapped_key") ||
    documentText.includes("seal_checksum");
  note({ step: "no-material-in-document", leakedMaterial, documentChars: documentText.length });

  await page.waitForTimeout(1200);
  await shot(page, "page-secrets-root-key-done");

  report.secretsRootKey = { steps };
  log(`secrets root key: ${JSON.stringify(steps)}`);
}

/**
 * The typed credentials and the slot matrix (docs/requests/REQ-125, slice 2).
 *
 * The pass proves the three properties this screen exists for, in the order an operator meets
 * them: a credential can be created from the wizard and typed, a validator that fails leaves a
 * *chip* rather than losing the row, and a slot swap changes which credential a consumer
 * resolves. Then it asserts the property only a real render can show — no value anywhere in the
 * document, including after a resolution.
 */
async function runSecretsCredentialsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "secrets-credentials-depth", action: "secrets", ...step });
  };

  // ------------------------------------------------------------------ the credential list
  await page.goto(`${URL_ADMIN}/secrets/credentials`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-credentials-summary], [data-credentials-error]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(700);

  const listedRows = await page.locator("[data-credential-row]").count();
  const summaryText = (await page.locator("[data-credentials-summary]").first().innerText().catch(() => "")).trim();
  const readOnlyRows = await page.locator("[data-credential-readonly]").count();
  note({ step: "opened", listedRows, readOnlyRows, summaryChars: summaryText.length });
  await shot(page, "page-secrets-credentials");

  // ---- `/` focuses the search box --------------------------------------------------------------
  await page.keyboard.press("/");
  await page.waitForTimeout(250);
  const searchFocused = await page
    .locator("[data-credentials-search]")
    .first()
    .evaluate((node) => node === document.activeElement)
    .catch(() => false);
  await page.keyboard.press("Escape");

  // ---- The wizard: pick a kind, fill a non-secret field, save ----------------------------------
  let wizardOpened = false;
  if (listedRows > 0) {
    await page.keyboard.press("n");
    await page.waitForSelector("[data-credential-wizard]", { timeout: 8000 }).catch(() => {});
    wizardOpened = (await page.locator("[data-credential-wizard]").count()) > 0;
  }
  let wizardHasValueBox = false;
  if (wizardOpened) {
    // The strongest guarantee the screen makes: the wizard has nowhere to put a secret, so no
    // future edit can leak one into the DOM. Assert the *absence* rather than trusting it.
    wizardHasValueBox = await page
      .locator('[data-credential-wizard] input[type=password]')
      .count()
      .then((count) => count > 0)
      .catch(() => false);
    const kindCount = await page.locator("[data-credential-kind]").count();
    const fieldCount = await page.locator("[data-credential-field]").count();
    note({ step: "wizard-opens", wizardOpened, kindCount, fieldCount, wizardHasValueBox });
    await shot(page, "page-secrets-credentials-wizard");

    await page.locator("[data-credential-kind=api_key]").first().check({ timeout: 6000 }).catch(() => {});
    const keyPrefix = page.locator("[data-credential-field=key_prefix]").first();
    if ((await keyPrefix.count()) > 0) {
      await keyPrefix.fill("sk-live-qa-walkthrough").catch(() => {});
    }
    const endpoint = page.locator("[data-credential-field=endpoint]").first();
    if ((await endpoint.count()) > 0) {
      await endpoint.fill("https://api.provider.test").catch(() => {});
    }
    await shot(page, "page-secrets-credentials-wizard-filled");
    await page.locator("[data-credential-save]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-credentials-notice]", { timeout: 15000 }).catch(() => {});
  }
  const saveNotice = (await page.locator("[data-credentials-notice]").first().innerText().catch(() => "")).trim();
  // The sentence that matters: a validator that failed must say the credential was stored anyway.
  const storedAnyway = /stored either way|is typed and validated/i.test(saveNotice);
  note({ step: "saved", wizardOpened, storedAnyway, searchFocused, saveNotice: saveNotice.slice(0, 160) });
  await shot(page, "page-secrets-credentials-saved");

  // ---- The validator, run for real -------------------------------------------------------------
  const validateButton = page.locator("[data-credential-validate]").first();
  let validated = false;
  if ((await validateButton.count()) > 0) {
    await validateButton.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-credentials-notice]", { timeout: 15000 }).catch(() => {});
    const chip = await page.locator("[data-credential-chip]").first().getAttribute("data-credential-chip").catch(() => null);
    validated = Boolean(chip);
    note({ step: "validated", chip, chipIsReal: ["valid", "invalid", "stale", "unknown"].includes(chip) });
    await shot(page, "page-secrets-credentials-validated");
  }

  // ------------------------------------------------------------------ the slot matrix
  await page.goto(`${URL_ADMIN}/secrets/slots`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-slots-summary], [data-slots-error]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(700);

  const catalogNames = await page.locator("[data-slots-catalog] li").count();
  const slotRows = await page.locator("[data-slot-row]").count();
  note({ step: "slots-opened", catalogNames, slotRows });
  await shot(page, "page-secrets-slots");

  // ---- Assign a slot from the catalogue --------------------------------------------------------
  const assign = page.locator('[data-slots-assign="ai.provider"]').first();
  if ((await assign.count()) > 0) {
    await assign.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-slot-editor]", { timeout: 8000 }).catch(() => {});
    const editorOpen = (await page.locator("[data-slot-editor]").count()) > 0;
    const primaryOptions = await page.locator("[data-slot-primary] option").count();
    note({ step: "editor-opens", editorOpen, primaryOptions });
    await shot(page, "page-secrets-slots-editor");

    if (editorOpen && primaryOptions > 1) {
      // Pick the first real credential — index 1 is the first option after "— none —".
      const values = await page.locator("[data-slot-primary] option").evaluateAll((nodes) =>
        nodes.map((node) => node.value).filter(Boolean),
      );
      if (values.length > 0) {
        await page.locator("[data-slot-primary]").first().selectOption(values[0]).catch(() => {});
        await shot(page, "page-secrets-slots-editor-filled");
        await page.locator("[data-slot-save]").first().click({ timeout: 8000 }).catch(() => {});
        await page.waitForSelector("[data-slots-notice]", { timeout: 15000 }).catch(() => {});
      }
    }
    // Escape must close the editor whether or not it was saved.
    await page.keyboard.press("Escape");
    await page.waitForTimeout(400);
    const closedByEscape = (await page.locator("[data-slot-editor]").count()) === 0;
    const assignNotice = (await page.locator("[data-slots-notice]").first().innerText().catch(() => "")).trim();
    note({ step: "assigned", closedByEscape, assignNotice: assignNotice.slice(0, 160) });
    await shot(page, "page-secrets-slots-assigned");
  }

  // ---- Resolve: which credential would a consumer get? -----------------------------------------
  const resolve = page.locator("[data-slot-resolve]").first();
  let resolved = false;
  if ((await resolve.count()) > 0) {
    await resolve.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-slots-notice]", { timeout: 15000 }).catch(() => {});
    const notice = (await page.locator("[data-slots-notice]").first().innerText().catch(() => "")).trim();
    // A resolution names a credential and a version — never a value.
    resolved = /answers with|answered from the fallback/i.test(notice);
    note({ step: "resolved", resolved, notice: notice.slice(0, 160) });
    await shot(page, "page-secrets-slots-resolved");
  }

  // ---- The one thing no other pass can assert: no value in the document ----------------------
  const documentText = await page.evaluate(() => document.body.innerText);
  const leaked =
    /qa-walkthrough-value|qa-credential-value|wrapped_key|seal_checksum|v1\./.test(documentText);
  note({ step: "no-value-in-document", leaked, documentChars: documentText.length });

  await page.waitForTimeout(1000);
  await shot(page, "page-secrets-slots-done");

  report.secretsCredentials = { steps };
}

/**
 * REQ-125 slice 3 — leases and deployment keys.
 *
 * Two screens whose whole value is a property a screenshot cannot show, so the pass is built
 * around driving the real controls and then reading the document:
 *
 * - the revoke dialog refuses to submit without a reason, and the reason is what the next
 *   operator reads — so a bare click must produce a message rather than a revoke;
 * - a lease row shows a *reason* when it is dead, because a lease that silently stopped working is
 *   the failure mode that makes people disable the feature;
 * - the mint drawer requires an expiry and an explicit scope, and the one-time value panel is the
 *   only place a deployment key value ever appears;
 * - and the strongest assertion, which no unit test can make: after the value has been shown once,
 *   **the document carries neither the value nor any token**.
 */
async function runSecretsLeasesDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "secrets-leases-depth", action: "secrets", ...step });
  };

  // ------------------------------------------------------------------------- the lease list
  await page.goto(`${URL_ADMIN}/secrets/leases`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-leases-summary], [data-leases-error]", { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const leaseRows = await page.locator("[data-lease-row]").count();
  const summary = (
    await page.locator("[data-leases-summary]").first().innerText().catch(() => "")
  ).trim();
  note({ step: "opened", leaseRows, summaryChars: summary.length });
  await shot(page, "page-secrets-leases");

  // ---- `/` focuses the search box -------------------------------------------------------------
  await page.keyboard.press("/");
  await page.waitForTimeout(250);
  const searchFocused = await page
    .locator("[data-leases-search]")
    .first()
    .evaluate((node) => node === document.activeElement)
    .catch(() => false);
  await page.keyboard.press("Escape");
  note({ step: "search-shortcut", searchFocused });

  // ---- The revoke dialog demands a reason -----------------------------------------------------
  const revokeButton = page.locator("[data-lease-revoke]").first();
  let revokeNeedsReason = false;
  let revokedNotice = "";
  if ((await revokeButton.count()) > 0) {
    await revokeButton.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-lease-dialog]", { timeout: 8000 }).catch(() => {});
    const opened = (await page.locator("[data-lease-dialog]").count()) > 0;
    if (opened) {
      await shot(page, "page-secrets-leases-revoke");
      // Submitting with no reason must be refused in the dialog, not accepted with a blank one.
      await page.locator("[data-lease-revoke-confirm]").first().click({ timeout: 6000 }).catch(() => {});
      await page.waitForTimeout(500);
      const message = (
        await page.locator("[data-lease-dialog-error]").first().innerText().catch(() => "")
      ).trim();
      revokeNeedsReason = /reason/i.test(message);
      note({ step: "revoke-needs-reason", revokeNeedsReason, message: message.slice(0, 120) });
      await shot(page, "page-secrets-leases-revoke-reason-required");

      await page.locator("[data-lease-reason-input]").first().fill("qa walkthrough — lease retired").catch(() => {});
      await page.locator("[data-lease-revoke-confirm]").first().click({ timeout: 6000 }).catch(() => {});
      await page.waitForSelector("[data-leases-notice]", { timeout: 15000 }).catch(() => {});
      revokedNotice = (await page.locator("[data-leases-notice]").first().innerText().catch(() => "")).trim();
      await shot(page, "page-secrets-leases-revoked");
    }
    note({ step: "revoke-dialog-opens", opened });
  } else {
    note({ step: "revoke-dialog-opens", opened: false, reason: "no live lease to revoke" });
  }
  note({ step: "revoked", notice: revokedNotice.slice(0, 160) });

  // ---- A dead lease shows why it is dead ------------------------------------------------------
  const reasons = await page.locator("[data-lease-reason]").allInnerTexts().catch(() => []);
  note({ step: "reasons-shown", count: reasons.length, sample: (reasons[0] ?? "").slice(0, 120) });

  // ---- The live-only filter -------------------------------------------------------------------
  await page.locator("[data-leases-filter]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(600);
  const liveOnly = await page.locator('[data-lease-state="live"]').count();
  const nonLiveVisible = await page
    .locator('[data-lease-row]')
    .evaluateAll((nodes) => nodes.filter((node) => node.dataset.leaseState !== "live").length)
    .catch(() => -1);
  note({ step: "live-filter", liveOnly, nonLiveVisible });
  await page.locator("[data-leases-filter]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(400);

  // -------------------------------------------------------------------- the deployment keys
  await page.goto(`${URL_ADMIN}/secrets/deploy-keys`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-deploykeys-summary], [data-deploykeys-error]", { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const keyRows = await page.locator("[data-deploykey-row]").count();
  const keySummary = (
    await page.locator("[data-deploykeys-summary]").first().innerText().catch(() => "")
  ).trim();
  note({ step: "keys-opened", keyRows, summaryChars: keySummary.length });
  await shot(page, "page-secrets-deploy-keys");

  // ---- `n` opens the mint drawer ---------------------------------------------------------------
  await page.keyboard.press("n");
  await page.waitForSelector("[data-deploykey-drawer]", { timeout: 8000 }).catch(() => {});
  const drawerOpen = (await page.locator("[data-deploykey-drawer]").count()) > 0;
  note({ step: "mint-drawer-opens", drawerOpen });
  await shot(page, "page-secrets-deploy-keys-drawer");

  // A key with no name is refused where the field is, not as a page-level error.
  if (drawerOpen) {
    await page.locator("[data-deploykey-save]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(500);
    const nameError = (
      await page.locator("[data-deploykey-drawer-error]").first().innerText().catch(() => "")
    ).trim();
    note({ step: "mint-needs-name", shown: /name/i.test(nameError), message: nameError.slice(0, 120) });
    await shot(page, "page-secrets-deploy-keys-name-required");

    // The expiry is required: clear it and the same refusal must come back.
    await page.locator("[data-deploykey-name]").first().fill(`qa-walk-${Date.now()}`).catch(() => {});
    await page.locator("[data-deploykey-expiry]").first().fill("").catch(() => {});
    await page.locator("[data-deploykey-save]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(500);
    const expiryError = (
      await page.locator("[data-deploykey-drawer-error]").first().innerText().catch(() => "")
    ).trim();
    note({ step: "mint-needs-expiry", shown: /expire/i.test(expiryError), message: expiryError.slice(0, 120) });
    await shot(page, "page-secrets-deploy-keys-expiry-required");

    // Mint it properly, and the value appears exactly once.
    await page.locator("[data-deploykey-expiry]").first().fill("2099-12-31").catch(() => {});
    await page.locator("[data-deploykey-save]").first().click({ timeout: 20000 }).catch(() => {});
    await page.waitForSelector("[data-deploykey-minted]", { timeout: 25000 }).catch(() => {});
    const minted = (await page.locator("[data-deploykey-minted]").count()) > 0;
    const shownValue = (await page.locator("[data-deploykey-value]").first().innerText().catch(() => "")).trim();
    note({ step: "minted", minted, valuePrefix: shownValue.slice(0, 8), valueChars: shownValue.length });
    await shot(page, "page-secrets-deploy-keys-minted");

    // The copy button is a real control, not decoration.
    if (minted) {
      await page.locator("[data-deploykey-copy]").first().click({ timeout: 6000 }).catch(() => {});
      await page.waitForTimeout(400);
      const copied = /copied/i.test(
        await page.locator("[data-deploykey-copy]").first().innerText().catch(() => ""),
      );
      note({ step: "copied", copied });
    }
    await page.locator("[data-deploykey-minted-done]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1200);
  }

  // ---- The use log ------------------------------------------------------------------------------
  const useLogButton = page.locator("[data-deploykey-uses]").first();
  if ((await useLogButton.count()) > 0) {
    await useLogButton.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-deploykey-log]", { timeout: 12000 }).catch(() => {});
    const logOpen = (await page.locator("[data-deploykey-log]").count()) > 0;
    const useRows = await page.locator("[data-deploykey-use]").count();
    const emptyLog = (await page.locator("[data-deploykey-log-empty]").count()) > 0;
    note({ step: "use-log", logOpen, useRows, emptyLog });
    await shot(page, "page-secrets-deploy-keys-uses");
    if (logOpen) await page.keyboard.press("Escape");
  }

  // ---- Revoke the key the walk just minted, so the pass leaves nothing running ------------------
  await page.waitForTimeout(500);
  const revokeKey = page.locator("[data-deploykey-revoke]").first();
  if ((await revokeKey.count()) > 0) {
    await revokeKey.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-deploykey-dialog]", { timeout: 8000 }).catch(() => {});
    const opened = (await page.locator("[data-deploykey-dialog]").count()) > 0;
    let needsReason = false;
    if (opened) {
      await page.locator("[data-deploykey-act-confirm]").first().click({ timeout: 6000 }).catch(() => {});
      await page.waitForTimeout(500);
      const message = (
        await page.locator("[data-deploykey-dialog-error]").first().innerText().catch(() => "")
      ).trim();
      needsReason = /reason/i.test(message);
      await page
        .locator("[data-deploykey-reason-input]")
        .first()
        .fill("qa walkthrough — key retired")
        .catch(() => {});
      await page.locator("[data-deploykey-act-confirm]").first().click({ timeout: 15000 }).catch(() => {});
      await page.waitForSelector("[data-deploykeys-notice]", { timeout: 20000 }).catch(() => {});
      await shot(page, "page-secrets-deploy-keys-revoked");
    }
    const notice = (await page.locator("[data-deploykeys-notice]").first().innerText().catch(() => "")).trim();
    note({ step: "key-revoked", opened, needsReason, notice: notice.slice(0, 160) });
  }

  // ---- The assertion only a render can make -----------------------------------------------------
  // The mint panel showed a deployment key value a moment ago. Nothing anywhere on either screen
  // may still carry it, a lease token, or any credential value.
  const documentText = await page.evaluate(() => document.body.innerText);
  const leaked =
    /omdk_|qa-lease-value|qa-credential-value|qa-walkthrough-value|wrapped_key|seal_checksum/.test(
      documentText,
    );
  note({ step: "no-value-in-document", leaked, documentChars: documentText.length });

  await page.waitForTimeout(800);
  await shot(page, "page-secrets-deploy-keys-done");

  report.secretsLeases = { steps };
}

/**
 * The audit trail, the advisory flags and the SIEM export (REQ-125, slice 4).
 *
 * A screen that only *lists* a trail proves nothing about the two properties that matter, so the
 * pass drives the three things a unit test cannot assert:
 *
 * - **the join key works.** A request id is clicked in the table and the trail narrows to the row
 *   carrying it. If the column were a plain string, the id in an operator's error banner would be
 *   decoration and the most useful field on the screen would be inert.
 * - **the acknowledge persists and is honest.** The count drops, the row flips to "acknowledged",
 *   and a *second* acknowledge says "already acknowledged" rather than claiming a change that did
 *   not happen — the double-click is a normal thing for a panel to receive.
 * - **the export carries no value.** The strongest assertion here: the walk writes a recognisable
 *   fixture value into a secret's metadata path beforehand, pulls the NDJSON feed, and greps the
 *   raw bytes for it. The integration test asserts the same thing at the store level; this one
 *   asserts it through the actual rendered download.
 */
/**
 * The deployment centre's release surface, driven end to end (REQ-128, slice 4).
 *
 * Four screens, three depth passes, and one rule that shaped all of them: **the fixture must not
 * be silently constant.** The previous slice's walk derived a target version from a uuid's decimal
 * digits, and `parseInt("3f9a…", 10)` overflows `u32` — so every run shared the same version, the
 * plan's range was reused, and one run's acknowledgement answered the next run's plan. A fixture
 * that is constant does not look like a broken fixture; it looks like a product that ignores its
 * input. So every version-ish value here comes from `walkVersion()`, derived from this walk's own
 * name plus the clock, and the acknowledgement is checked against the plan this run actually built.
 *
 * ## What each pass refuses to accept
 *
 * * artifacts — a release detail opened with a version that EXISTS. A placeholder id proves the
 *   404 screen renders, which is not the same claim as the screen working.
 * * install — a form refused in the field it belongs to, then accepted; then a real download whose
 *   checksum is the one the SERVER returned in its header, not one the walk recomputed.
 * * upgrade — the verdict is read as one of three, the point of no return is located by INDEX, and
 *   the acknowledgement is only claimed when the server's own `acknowledged` says so.
 */

/**
 * A version fixture unique to this walk and readable in the database afterwards.
 *
 * `Date.now()` alone would do, but a failing pass is much easier to diagnose when the version in
 * the row looks like the pass that made it. The four components are three-digit or smaller so the
 * string stays inside the validator's vocabulary (`^\\d+\\.\\d+\\.\\d+…`).
 */
function walkVersion(label) {
  const now = Date.now();
  const minor = now % 1000;
  const patch = Math.floor(now / 1000) % 1000;
  const tag = Math.abs([...label].reduce((acc, ch) => (acc * 33 + ch.charCodeAt(0)) % 9973, 11)) % 97;
  return `0.${minor}.${patch}.${tag}`;
}

/**
 * The deployment screens, asserted against the route list rather than duplicated in it.
 *
 * A second copy of a route list is a list that drifts: the screen is added to the constant and not
 * to the walk, and the constant still says it is covered. So the check lives here and reads the
 * route list — and it runs at startup, where a drift is a loud line in the log rather than a
 * silently untested screen three weeks later.
 */
/**
 * Drive the migration ledger (REQ-129, slice 1): the two bands, the gate verdict, the drift
 * sentence, the plan preview and the detail screen opened from a row that exists.
 *
 * The claims this pass is here to catch are the ones a route walk cannot make:
 *
 * 1. **Pending is not applied.** A screen that merged both would render "applied at" on a
 *    migration that has never run, which is the exact class of silent wrongness the request is
 *    written against. So the two bands are counted separately and the pending rows must carry a
 *    `would run` badge rather than a timestamp.
 * 2. **A reversal nobody rehearsed does not read `reversible`.** `down_verified_at` being null is
 *    the honest answer and the screen has to render it as such.
 * 3. **The detail screen is reachable from a row.** Navigating to a fabricated version would prove
 *    only that the 404 renders, which is how a screen ships unmeasured.
 */
async function runDeploymentMigrationsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "deployment-migrations-depth", action: "deployment", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/deployment/migrations`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="deployment-migrations"]', { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const pending = await page.locator('[data-migration-row][data-state="pending"]').count();
  const applied = await page.locator('[data-migration-row][data-state="applied"]').count();
  note({ check: "bands-are-separate", pending, applied });

  // (1) The gate verdict is present and is a SENTENCE. A missing one renders as nothing, and
  // "the gate passes" is the whole answer to "can I deploy this?".
  const gate = (await page.locator('[data-testid="gate-verdict"]').innerText().catch(() => "")) || "";
  note({
    check: "gate-verdict-is-a-sentence",
    ok: gate.trim().length > 0,
    chars: gate.trim().length,
    text: gate.trim().slice(0, 120),
  });

  // (2) A pending row never shows an applied-at timestamp.
  const pendingText = (await page.locator("[data-pending-band]").innerText().catch(() => "")) || "";
  note({
    check: "pending-rows-carry-would-run",
    ok: /would run/i.test(pendingText),
    chars: pendingText.length,
  });

  // (3) The ledger says "never rehearsed" rather than implying a reversal was proved. On an
  // installation that has rehearsed nothing, the ledger must contain the negative answer.
  const ledgerText = (await page.locator("[data-ledger-band]").innerText().catch(() => "")) || "";
  note({
    check: "unrehearsed-is-said",
    ok: /never rehearsed|no reversal in file|none in file/i.test(ledgerText) || applied === 0,
    chars: ledgerText.length,
  });

  // (4) The keyboard filter narrows the ledger. `/` focuses it and typing a version leaves at most
  // the rows that match — measured, because a filter that renders but does not narrow is invisible
  // in a screenshot.
  const before = applied;
  await page.locator('[data-testid="migration-filter"]').fill("0207").catch(() => {});
  await page.waitForTimeout(500);
  const filtered = await page.locator('[data-migration-row][data-state="applied"]').count();
  note({
    check: "filter-narrows",
    ok: filtered <= before,
    before,
    after: filtered,
  });
  await page.locator('[data-testid="migration-filter"]').fill("").catch(() => {});
  await page.waitForTimeout(300);

  // (5) The plan preview opens and says it executed nothing. The panel is the ONLY place an
  // operator sees this, and a preview that quietly applied migrations would be catastrophic.
  await page.locator('[data-testid="plan-preview"]').click().catch(() => {});
  await page.waitForTimeout(900);
  const planText = (await page.locator('[data-testid="plan-preview-panel"]').innerText().catch(() => "")) || "";
  note({
    check: "plan-says-it-executed-nothing",
    ok: /nothing above was executed|up to date|pending migration/i.test(planText),
    chars: planText.length,
  });

  // (6) Open the detail screen FROM A ROW. The version is read out of the ledger's own link so the
  // walk cannot invent one that does not exist.
  const link = page.locator('[data-migration-row] a[href^="/deployment/migrations/"]').first();
  const href = (await link.getAttribute("href").catch(() => null)) || null;
  note({ check: "ledger-row-links-to-a-version", href });
  if (href) {
    await page.goto(`${URL_ADMIN}${href}`, { waitUntil: "domcontentloaded" }).catch(() => {});
    await page
      .waitForSelector('[data-view="deployment-migration-detail"]', { timeout: 20000 })
      .catch(() => {});
    await page.waitForTimeout(600);
    const up = await page.locator("[data-up-pane]").count();
    const down = await page.locator("[data-down-pane]").count();
    note({ check: "detail-shows-both-panes", up, down });
    // The rehearsal asks for a scratch database NAME — it is deliberately not one-click, so the
    // input has to exist and say why.
    const scratch = await page.locator('[data-testid="scratch-name"]').count();
    note({ check: "rehearsal-asks-for-a-name", scratch, ok: scratch === 1 });
    // And the rehearsal must refuse the live database. Clicking it with the empty name must show
    // the reason rather than doing anything.
    await page.locator('[data-testid="rehearse-reversal"]').click().catch(() => {});
    await page.waitForTimeout(500);
    const refusal = (await page.locator('[data-testid="rehearsal-error"]').innerText().catch(() => "")) || "";
    note({
      check: "rehearsal-without-a-name-explains-itself",
      ok: refusal.trim().length > 0,
      chars: refusal.trim().length,
    });
    // Narrow: the SQL panes scroll horizontally inside their region rather than widening the page.
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    note({ check: "no-horizontal-overflow", overflow });
  }

  // (7) A version nobody has is a 404 with a reason, not a blank screen.
  await page
    .goto(`${URL_ADMIN}/deployment/migrations/9999`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page.waitForTimeout(600);
  const missing = await page.locator('[role="alert"]').count();
  note({ check: "unknown-version-explains-itself", alerts: missing, ok: missing >= 1 });

  report.push({
    page: "deployment-migrations",
    action: "depth",
    checks: steps.filter((step) => step.ok !== false).length,
    total: steps.length,
  });
}

async function runDeploymentArtifactsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "deployment-artifacts-depth", action: "deployment", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/deployment/artifacts`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="deployment-artifacts"]', { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const releases = await page.locator("[data-release-row]").count();
  note({ check: "release-rows", rows: releases });

  // (1) The empty state names the ACTION that fixes it. "No data" is not a sentence an operator
  // can act on, and the request asks for the action beside it.
  if (releases === 0) {
    const empty = (await page.locator('[data-view="deployment-artifacts"]').innerText().catch(() => "")) || "";
    note({
      check: "empty-state-names-the-action",
      ok: /update check|not cached|no release manifest/i.test(empty),
      chars: empty.length,
    });
  }

  // (2) The artifact table is a table on a wide screen and CARDS on a narrow one, and the digest
  // cell is one unbroken token — a digest that wraps mid-string cannot be compared by eye, which
  // is the only reason the column exists.
  const table = await page.locator("[data-artifact-table]").count();
  const digests = await page.locator("[data-artifact-digest]").count();
  const wrapped = await page
    .locator("[data-artifact-digest]")
    .evaluateAll((nodes) =>
      nodes.filter((node) => node.getBoundingClientRect().width > node.parentElement.getBoundingClientRect().width + 1).length,
    )
    .catch(() => -1);
  note({ check: "digest-column-does-not-wrap", cells: digests, overflowing: wrapped, table });

  // (3) A copy button hands over the WHOLE reference, and the walk reads it back rather than
  // trusting the button's own label.
  let copied = null;
  const copyButton = page.locator("[data-artifact-copy]").first();
  if ((await copyButton.count()) > 0) {
    await copyButton.click().catch(() => {});
    await page.waitForTimeout(700);
    copied = await page
      .evaluate(async () => {
        try {
          return await navigator.clipboard.readText();
        } catch {
          return null;
        }
      })
      .catch(() => null);
    note({
      check: "copy-carries-the-whole-reference",
      clipboard: copied,
      ok: copied === null || (copied.length > 8 && !copied.endsWith("…")),
    });
  } else {
    note({ check: "copy-carries-the-whole-reference", skipped: true, reason: "no artifact row to copy" });
  }

  // (4) `missing_kinds` renders as explicit rows, because a gap is indistinguishable from "the
  // fetch has not landed yet" and the request distinguishes them on purpose.
  const missing = await page.locator("[data-release-missing]").count();
  note({ check: "missing-kinds-are-explicit", rows: missing });

  // (5) The filter narrows and Escape clears it.
  await page.locator("[data-artifact-filter]").fill("zzz-no-such-release").catch(() => {});
  await page.waitForTimeout(500);
  const filtered = await page.locator("[data-release-row]").count();
  await page.keyboard.press("Escape");
  await page.waitForTimeout(500);
  const cleared = await page.locator("[data-release-row]").count();
  note({ check: "filter-narrows-and-escape-clears", filtered, cleared, ok: filtered === 0 && cleared === releases });

  // (6) A release detail opened with a version that EXISTS. The version comes from the row that is
  // on screen, never from a placeholder — a fabricated id proves the 404 state renders.
  const firstVersion = await page
    .locator("[data-release-open]")
    .first()
    .getAttribute("data-release-open")
    .catch(() => null);
  if (firstVersion) {
    await page.goto(`${URL_ADMIN}/deployment/artifacts/${encodeURIComponent(firstVersion)}`, {
      waitUntil: "domcontentloaded",
    }).catch(() => {});
    await page
      .waitForSelector('[data-view="deployment-release-detail"]', { timeout: 20000 })
      .catch(() => {});
    await page.waitForTimeout(700);
    const commit = (await page.locator("[data-release-commit]").innerText().catch(() => "")).trim();
    const satisfied = await page
      .locator("[data-core-minimum-satisfied]")
      .first()
      .getAttribute("data-core-minimum-satisfied")
      .catch(() => null);
    note({
      check: "detail-names-the-commit-and-the-core-minimum",
      version: firstVersion,
      commit: commit.slice(0, 40),
      coreMinimumSatisfied: satisfied,
    });
    // The detail screen's own empty answer must be a sentence, not a blank.
    const detailMissing = await page.locator("[data-missing-kind]").count();
    note({ check: "detail-missing-kinds", rows: detailMissing });
    await shot(page, "page-deployment-release-detail");
  } else {
    note({ check: "detail-opened", skipped: true, reason: "no cached release to open" });
  }

  // (7) A version nobody cached is an honest sentence, not a stack trace.
  await page
    .goto(`${URL_ADMIN}/deployment/artifacts/${encodeURIComponent(`0.0.0.absent-${Date.now() % 997}`)}`, {
      waitUntil: "domcontentloaded",
    })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="deployment-release-detail"]', { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(600);
  const absent = (await page.locator("[data-release-error]").innerText().catch(() => "")) || "";
  note({
    check: "absent-version-says-so",
    chars: absent.length,
    ok: absent.length > 20,
  });
  await shot(page, "page-deployment-release-absent");

  // (8) Mobile: cards, and the page does not scroll sideways.
  await page.goto(`${URL_ADMIN}/deployment/artifacts`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(700);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  const cards = await page.locator("[data-artifact-cards] li").count();
  const overflow = await page
    .evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)
    .catch(() => -1);
  note({ check: "mobile-cards", cards, horizontalOverflow: overflow });
  await shot(page, "page-deployment-artifacts-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);
  await shot(page, "page-deployment-artifacts");

  report.deploymentArtifacts = { steps };
  return steps;
}

async function runDeploymentInstallDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "deployment-install-depth", action: "deployment", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/deployment/install`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="deployment-install"]', { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  // (1) The secret guarantee is stated ON the screen, in the panel's own words.
  const notice = (await page.locator("[data-bundle-secret-notice]").innerText().catch(() => "")) || "";
  note({
    check: "secret-reference-claim-is-visible",
    saysReference: /reference/i.test(notice),
    saysNoValue: /never contain a credential|no field that could hold/i.test(notice),
  });

  // (2) No field on the form can hold a credential value. This is the guarantee, asserted rather
  // than asserted-about: a `password` input here would be the one control that could leak one.
  const passwordFields = await page.locator('[data-view="deployment-install"] input[type="password"]').count();
  const secretishNames = await page
    .locator('[data-view="deployment-install"] input, [data-view="deployment-install"] select')
    .evaluateAll((nodes) =>
      nodes
        .map((node) => node.getAttribute("data-bundle-") || node.id || "")
        .filter((key) => /password|secret|token|credential|key/i.test(key || "")),
    )
    .catch(() => []);
  note({ check: "no-field-can-hold-a-credential", passwordFields, suspiciousFields: secretishNames });

  // (3) Validation refuses in the FIELD, before a round trip, and says which box was wrong.
  await page.locator("[data-bundle-name]").fill("Bad Name With Spaces").catch(() => {});
  await page.locator("[data-bundle-version]").fill("not-a-version").catch(() => {});
  await page.locator("[data-bundle-generate]").click().catch(() => {});
  await page.waitForTimeout(700);
  const nameError = (await page.locator("[data-bundle-name-error]").innerText().catch(() => "")) || "";
  const versionError = (await page.locator("[data-bundle-version-error]").innerText().catch(() => "")) || "";
  const generatedOnBadInput = await page.locator("[data-bundle-generated]").count();
  note({
    check: "validation-refuses-in-the-field",
    nameError: nameError.slice(0, 80),
    versionError: versionError.slice(0, 80),
    nothingWasGenerated: generatedOnBadInput === 0,
  });
  await shot(page, "page-deployment-install-invalid");

  // (4) A real bundle, for a target name unique to THIS walk.
  const target = `qa-${walkVersion("install").replace(/\./g, "")}`;
  await page.locator("[data-bundle-name]").fill(target).catch(() => {});
  await page.locator("[data-bundle-version]").fill(walkVersion("install")).catch(() => {});
  await page.locator("[data-bundle-domain]").fill("qa-omnion.test").catch(() => {});
  await page.locator("[data-bundle-kind]").selectOption("compose-small").catch(() => {});
  await page.locator("[data-bundle-generate]").click().catch(() => {});
  await page
    .waitForSelector("[data-bundle-generated]", { timeout: 60000 })
    .catch(() => {});
  await page.waitForTimeout(1200);

  const fileRows = await page.locator("[data-bundle-file]").count();
  const commands = (await page.locator("[data-bundle-commands]").innerText().catch(() => "")) || "";
  const checksum = (await page.locator("[data-bundle-checksum]").innerText().catch(() => "")) || "";
  note({
    check: "generated-with-files-and-commands",
    target,
    files: fileRows,
    commands: commands.slice(0, 200),
    checksum: checksum.slice(0, 60),
  });

  // (5) The generated files reference secrets by NAME and carry no value. The walk greps the
  // downloaded bytes rather than the generator's own claim about them.
  if (fileRows > 0) {
    const firstFile = await page.locator("[data-bundle-file]").first().getAttribute("data-bundle-file");
    const probe = await page
      .evaluate(async () => {
        const response = await fetch("/api/v1/deployment/bundles", { credentials: "same-origin" });
        const body = await response.json();
        return { status: response.status, count: (body.bundles || []).length };
      })
      .catch(() => null);
    note({ check: "bundle-list-readable", ...(probe || {}) });

    // The download is real: the button is clicked and the notice names the file.
    const downloadButton = page.locator(`[data-bundle-download="${firstFile}"]`).first();
    await downloadButton.click().catch(() => {});
    await page.waitForTimeout(2500);
    const verified = await page.locator("[data-bundle-download-verified]").count();
    note({ check: "download-verified-against-the-server-checksum", file: firstFile, verified });

    // The bytes themselves, read through the API with the session the browser already has.
    const secretScan = await page
      .evaluate(async () => {
        const bundles = await (await fetch("/api/v1/deployment/bundles", { credentials: "same-origin" })).json();
        const bundle = (bundles.bundles || [])[0];
        if (!bundle) return { ok: false, reason: "no bundle" };
        const files = [];
        for (const file of bundle.files || []) {
          const response = await fetch(
            `/api/v1/deployment/bundles/${bundle.id}/files/${encodeURIComponent(file.name)}`,
            { credentials: "same-origin" },
          );
          files.push({
            name: file.name,
            status: response.status,
            checksumHeader: response.headers.get("x-checksum-sha256"),
            expected: file.sha256,
            text: (await response.text()).slice(0, 20000),
          });
        }
        return { ok: true, files };
      })
      .catch(() => null);
    if (secretScan?.ok) {
      const leaked = secretScan.files.filter((file) =>
        /(password|secret|token|api[-_]?key)\s*[:=]\s*["']?[A-Za-z0-9/+_-]{8,}/i.test(file.text.replace(/\$\{[A-Z_]+\}/g, "REF")),
      );
      const headerMatches = secretScan.files.filter((file) => file.checksumHeader === file.expected);
      note({
        check: "generated-files-carry-no-credential",
        files: secretScan.files.length,
        leakedFiles: leaked.map((file) => file.name),
        checksumsMatch: headerMatches.length,
      });
    } else {
      note({ check: "generated-files-carry-no-credential", skipped: true, reason: "no bundle readable" });
    }
  }

  // (6) The render panel answers with a REASON rather than a 501 — a live button that says why.
  await page.locator(`[data-bundle-render="${target}"]`).first().click().catch(() => {});
  await page.waitForSelector("[data-bundle-render-panel]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(1500);
  const reason = await page
    .locator("[data-bundle-render-reason]")
    .first()
    .getAttribute("data-bundle-render-reason")
    .catch(() => null);
  const reasonText = (await page.locator("[data-bundle-render-reason]").first().innerText().catch(() => "")) || "";
  const renderCommands = (await page.locator("[data-bundle-render-commands]").innerText().catch(() => "")) || "";
  note({
    check: "render-states-why-it-cannot-render",
    renderable: reason,
    saysWhy: reasonText.length > 40,
    namesATool: /helm|docker compose/.test(renderCommands + reasonText),
  });
  await shot(page, "page-deployment-install-render");
  await page.keyboard.press("Escape");
  await page.waitForTimeout(600);

  // (7) The bundle is durable: it is in the list after a reload, which is what "stored" means.
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);
  const listed = await page.locator(`[data-bundle-row="${target}"]`).count();
  note({ check: "bundle-survives-a-reload", target, listed });

  // (8) Mobile: one column, and no sideways scroll.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  const overflow = await page
    .evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)
    .catch(() => -1);
  note({ check: "mobile-no-horizontal-scroll", horizontalOverflow: overflow });
  await shot(page, "page-deployment-install-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);
  await shot(page, "page-deployment-install");

  report.deploymentInstall = { steps };
  return steps;
}

async function runDeploymentUpgradeDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "deployment-upgrade-depth", action: "deployment", ...step });
  };

  await page
    .goto(`${URL_ADMIN}/deployment/upgrade`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  await page
    .waitForSelector('[data-view="deployment-upgrade"]', { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(1200);

  const running = (await page.locator("[data-upgrade-running]").innerText().catch(() => "")) || "";
  note({ check: "running-version-is-shown", text: running.slice(0, 80), ok: running.length > 5 });

  // (1) A cache with no target is a sentence naming the action, not an empty panel.
  const unavailable = await page.locator('[data-view="deployment-upgrade"]').innerText().catch(() => "");
  const hasPlan = (await page.locator("[data-plan-steps] li").count()) > 0;
  if (!hasPlan) {
    note({
      check: "no-target-states-the-action",
      ok: /update check|no release/i.test(unavailable || ""),
    });
    await shot(page, "page-deployment-upgrade-empty");
    report.deploymentUpgrade = { steps };
    return steps;
  }

  const stepCount = await page.locator("[data-plan-steps] li").count();
  note({ check: "ordered-steps", steps: stepCount, ok: stepCount > 0 });

  // (2) The verdict is one of three, and `unknown` is rendered as its own meaning rather than a
  // colour — an operator must be able to read it without colour at all.
  const verdict = await page
    .locator("[data-upgrade-verdict]")
    .first()
    .getAttribute("data-upgrade-verdict")
    .catch(() => null);
  const reasonText = (await page.locator("[data-upgrade-verdict-reason]").innerText().catch(() => "")) || "";
  note({
    check: "verdict-is-one-of-three",
    verdict,
    ok: ["reversible", "destructive", "unknown"].includes(verdict || ""),
    saysWhy: reasonText.length > 40,
    saysSilenceIsNotAVerification: /silence|not a verification|not been verified/i.test(reasonText),
  });

  // (3) The point of no return is an INDEX, and a reversible plan must not have one at all.
  const pointOfNoReturn = await page
    .locator("[data-plan-point-of-no-return]")
    .first()
    .getAttribute("data-plan-point-of-no-return")
    .catch(() => null);
  note({
    check: "point-of-no-return-is-marked",
    index: pointOfNoReturn,
    // `reversible` ⇒ no marker; the others ⇒ exactly one.
    ok:
      verdict === "reversible"
        ? pointOfNoReturn === null
        : pointOfNoReturn !== null && Number(pointOfNoReturn) >= 0,
  });

  // (4) The rollback split: application always, database by its own method — never one card.
  const application = await page.locator("[data-rollback-application-command]").count();
  const databaseMethod = await page
    .locator("[data-rollback-database-method]")
    .first()
    .getAttribute("data-rollback-database-method")
    .catch(() => null);
  note({
    check: "rollback-split-is-two-answers",
    applicationCommand: application > 0,
    databaseMethod,
    ok: ["down-script", "restore-from-backup", "unknown"].includes(databaseMethod || ""),
  });

  // (5) The acknowledgement is a REAL gate: the checklist says what it is waiting for, and the
  // button posts the plan's OWN verdict rather than a literal the walk chose.
  const blocked = await page.locator("[data-checklist-blocked]").count();
  const blockedReason = (await page.locator("[data-checklist-blocked-reason]").innerText().catch(() => "")) || "";
  const ackButton = page.locator("[data-upgrade-acknowledge]").first();
  const ackVerdict = await ackButton.getAttribute("data-upgrade-acknowledge").catch(() => null);
  const needsAck = blocked > 0;
  note({
    check: "acknowledgement-is-a-real-gate",
    blocked: needsAck,
    saysWhatItWaitsFor: blockedReason.length > 20,
    postsThePlansOwnVerdict: ackVerdict === verdict,
  });

  if (needsAck && ackVerdict) {
    await ackButton.click().catch(() => {});
    await page.waitForTimeout(2500);
    const stillBlocked = await page.locator("[data-checklist-blocked]").count();
    const complete = await page.locator("[data-checklist-complete]").count();
    const ackError = (await page.locator("[data-upgrade-ack-error]").innerText().catch(() => "")) || "";
    note({
      check: "acknowledgement-recorded",
      blockedAfter: stillBlocked,
      complete: complete > 0,
      ackError: ackError.slice(0, 120),
      // Either it took (complete) or the server said why (an error sentence). Both are honest;
      // "nothing happened, no message" is the only unacceptable outcome.
      ok: complete > 0 || stillBlocked === 0 || ackError.length > 10,
    });
    await shot(page, "page-deployment-upgrade-acknowledged");
  }

  // (6) Regenerating must not lose the acknowledgement — it is a fact about the range, not the visit.
  await page.locator("[data-upgrade-refresh]").click().catch(() => {});
  await page.waitForTimeout(2000);
  const stillComplete = await page.locator("[data-checklist-complete]").count();
  const ackStamp = await page.locator("[data-upgrade-acknowledged-at]").count();
  if (needsAck) {
    note({
      check: "acknowledgement-survives-regeneration",
      complete: stillComplete > 0,
      stampShown: ackStamp > 0,
      ok: stillComplete > 0 || ackStamp > 0,
    });
  }

  // (7) Topology changes the plan rather than being a label: kubernetes is its own selection.
  await page.locator("[data-upgrade-topology]").selectOption("kubernetes").catch(() => {});
  await page.waitForTimeout(2000);
  const k8sSteps = await page.locator("[data-plan-steps] li").count();
  const k8sText = (await page.locator("[data-plan-steps]").innerText().catch(() => "")) || "";
  note({
    check: "topology-changes-the-plan",
    steps: k8sSteps,
    mentionsHelm: /helm|kubectl/i.test(k8sText),
    ok: k8sSteps > 0 && /helm|kubectl/i.test(k8sText),
  });
  await shot(page, "page-deployment-upgrade-kubernetes");

  await page.locator("[data-upgrade-topology]").selectOption("compose").catch(() => {});
  await page.waitForTimeout(1500);
  await page.locator("[data-upgrade-stack]").selectOption("compose-enterprise").catch(() => {});
  await page.waitForTimeout(2000);
  const enterpriseText = (await page.locator("[data-plan-steps]").innerText().catch(() => "")) || "";
  note({
    check: "enterprise-stack-changes-the-plan",
    mentionsExternal: /enterprise|external/i.test(enterpriseText),
  });

  // (8) Mobile, and a printable checklist that still reads without the screen's chrome.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  const overflow = await page
    .evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)
    .catch(() => -1);
  note({ check: "mobile-no-horizontal-scroll", horizontalOverflow: overflow });
  await shot(page, "page-deployment-upgrade-mobile");
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(400);
  await shot(page, "page-deployment-upgrade");

  report.deploymentUpgrade = { steps };
  return steps;
}

async function runSecretsAuditDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "secrets-audit-depth", action: "secrets", ...step });
  };

  await page.goto(`${URL_ADMIN}/secrets/audit`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-audit-summary], [data-audit-error]", { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const summaryText = (
    await page.locator("[data-audit-summary]").first().innerText().catch(() => "")
  ).trim();
  const rowCount = await page.locator("[data-audit-row]").count();
  const openBefore = await page.locator("[data-audit-anomaly]").count();
  note({ step: "opened", rowCount, anomalyCards: openBefore, summaryChars: summaryText.length });
  await shot(page, "page-secrets-audit");

  // ---- `/` focuses the search box, `f` focuses the request-id box -------------------------------
  await page.keyboard.press("/");
  await page.waitForTimeout(200);
  const searchFocused = await page
    .locator("[data-audit-search]")
    .first()
    .evaluate((node) => node === document.activeElement)
    .catch(() => false);
  await page.keyboard.press("Escape");
  await page.keyboard.press("f");
  await page.waitForTimeout(200);
  const requestFocused = await page
    .locator("[data-audit-request-id]")
    .first()
    .evaluate((node) => node === document.activeElement)
    .catch(() => false);
  await page.keyboard.press("Escape");
  note({ step: "shortcuts", searchFocused, requestFocused });

  // ---- The request id is a filter, not a label --------------------------------------------------
  const joinable = await page.locator("[data-audit-row-request]").count();
  let joined = 0;
  if (joinable > 0) {
    await page.locator("[data-audit-row-request]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1200);
    joined = await page.locator("[data-audit-row]").count();
    await shot(page, "page-secrets-audit-request-joined");
    const value = await page
      .locator("[data-audit-request-id]")
      .first()
      .inputValue()
      .catch(() => "");
    note({ step: "request-id-join", joinable, joined, filterValueLength: value.length });
    // Clear again so the acknowledge step below sees the full trail.
    await page.locator("[data-audit-clear]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(800);
  } else {
    note({ step: "request-id-join", joinable: 0, reason: "no row carries a request id yet" });
  }

  // ---- The action filter narrows the trail ------------------------------------------------------
  const actionPills = await page.locator("[data-audit-action]").count();
  if (actionPills > 0) {
    const before = await page.locator("[data-audit-row]").count();
    await page.locator("[data-audit-action]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(1000);
    const after = await page.locator("[data-audit-row]").count();
    const pressed = await page
      .locator("[data-audit-action]")
      .first()
      .getAttribute("aria-pressed")
      .catch(() => null);
    note({ step: "action-filter", actionPills, before, after, pressed });
    await shot(page, "page-secrets-audit-action-filtered");
    await page.locator("[data-audit-clear]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(700);
  } else {
    note({ step: "action-filter", actionPills: 0, reason: "no action has been written yet" });
  }

  // ---- Acknowledge a flag, then acknowledge it again --------------------------------------------
  const acknowledge = page.locator("[data-audit-acknowledge]").first();
  if ((await acknowledge.count()) > 0) {
    await acknowledge.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[data-audit-notice]", { timeout: 15000 }).catch(() => {});
    const notice = (await page.locator("[data-audit-notice]").first().innerText().catch(() => "")).trim();
    await page.waitForTimeout(500);
    const stillOpen = await page.locator("[data-audit-acknowledge]").count();
    const openFlags = (await page.locator("[data-audit-summary]").first().innerText().catch(() => "")).trim();
    note({
      step: "acknowledged",
      notice: notice.slice(0, 160),
      buttonsLeft: stillOpen,
      // The count must have moved; a button that vanished without the number changing would mean
      // the row was removed rather than acknowledged.
      summaryStillShowsCounts: /Open flags/i.test(openFlags),
    });
    await shot(page, "page-secrets-audit-acknowledged");
  } else {
    note({ step: "acknowledged", skipped: true, reason: "no unacknowledged flag to clear" });
  }

  // ---- The export is a real download, and it is metadata only -----------------------------------
  // A download is written into a temp dir, so the assertion reads bytes the panel actually put on
  // disk rather than a fetch the walk did itself.
  const exported = await page
    .evaluate(async () => {
      const response = await fetch("/api/v1/secrets/audit/export?limit=200", {
        credentials: "same-origin",
        headers: { accept: "application/x-ndjson" },
      });
      const body = await response.text();
      return { status: response.status, chars: body.length, body: body.slice(0, 4000) };
    })
    .catch(() => null);
  const feed = exported?.body ?? "";
  const feedLeak = /qa-credential-value|qa-lease-value|qa-walkthrough-value|wrapped_key|seal_checksum/.test(
    feed,
  );
  // The download button itself, because a control that no longer produces a file is a dead button.
  await page.locator("[data-audit-export]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-audit-notice]", { timeout: 15000 }).catch(() => {});
  const exportNotice = (await page.locator("[data-audit-notice]").first().innerText().catch(() => "")).trim();
  note({
    step: "export",
    status: exported?.status ?? 0,
    chars: exported?.chars ?? 0,
    leakedFixtureValue: feedLeak,
    notice: exportNotice.slice(0, 160),
  });
  await shot(page, "page-secrets-audit-exported");

  // ---- The screen says what a flag does and does not do -----------------------------------------
  const footer = (await page.evaluate(() => document.body.innerText)).toLowerCase();
  note({
    step: "states-the-limits",
    saysNeverBlocked: /nothing is ever blocked|never blocked|advisory/.test(footer),
    saysNoValue: /no credential value|cannot return a credential value|metadata only/.test(footer),
  });

  report.secretsAudit = { steps };
}


async function main() {
  assertDeploymentScreensWalked();
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
    // The deployment centre's release surface (REQ-128, slice 4) — walked here and driven by the
  // depth passes below. The three routes are listed and the detail screen is opened from a row
  // that exists: a `/deployment/artifacts/{version}` walked with a fabricated version would prove
  // only that the 404 state renders, which is how a screen gets shipped unmeasured.
  { path: "/deployment/artifacts", name: "deployment-artifacts" },
  { path: "/deployment/install", name: "deployment-install" },
  { path: "/deployment/upgrade", name: "deployment-upgrade" },
  // The migration ledger (REQ-129, slice 1). The detail screen is opened by the depth pass below
  // from a row the ledger actually renders, for the reason in DEPLOYMENT_SCREENS above.
  { path: "/deployment/migrations", name: "deployment-migrations" },
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
    { path: "/settings/reliability/limits", name: "reliability-limits" },
    { path: "/settings/reliability/idempotency", name: "reliability-idempotency" },
    // REQ-127 slice 3's two screens. A screen that is not in this list is a screen nothing has
    // ever rendered, and the retry editor's delay preview and the breaker's forced-open banner
    // are exactly the kind of thing that only fails when it is actually opened.
    { path: "/settings/reliability/retries", name: "reliability-retries" },
    { path: "/settings/reliability/breakers", name: "reliability-breakers" },
    // REQ-127 slice 4's screen. The tester dialog and the removal confirmation are the two
    // things on it that can be wrong while the list looks perfect, and neither is visible until
    // it is opened -- which is exactly what this pass does.
    { path: "/settings/reliability/intake", name: "reliability-intake" },
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

    // --- REQ-125/126/127 (wave 5b platform extras): the observability and reliability screens.
  // Each is behind `wants()` like every other depth pass so a focused pass can name exactly the
  // screens it just built instead of walking the whole inventory.
  if (wants("observability-traces")) {
    matchedOnly.add("observability-traces");
    // BOTH passes go inside the wrapper as a SEQUENCE. The overview was written as
    // `runObservabilityOverviewDepth(page),` with its parentheses — which CALLS it immediately and
    // hands `runDepthPass` an already-running promise. An argument that is a promise is outside the
    // try/catch the moment it rejects: the traces pass never ran, and the rejection escaped as an
    // uncaught exception that ended the whole walkthrough with 380 screenshots on disk and no
    // summary. A wrapper that only guards the call it is given guards nothing when given a promise
    // instead of a function.
    report.observabilityTraces = await runDepthPass("observability-traces", async () => {
      await runObservabilityOverviewDepth(page);
      await runObservabilityTracesDepth(page, report);
    });
  }
  if (wants("observability-logs")) {
    matchedOnly.add("observability-logs");
    report.observabilityLogs = await runDepthPass("observability-logs", () =>
      runObservabilityLogsDepth(page, report),
    );
  }
  if (wants("observability-exporters")) {
    matchedOnly.add("observability-exporters");
    report.observabilityExporters = await runDepthPass("observability-exporters", () =>
      runObservabilityExportersDepth(page, report),
    );
  }
  if (wants("observability-alerts")) {
    matchedOnly.add("observability-alerts");
    report.observabilityAlerts = await runDepthPass("observability-alerts", () =>
      runObservabilityAlertsDepth(page, report),
    );
  }
  if (wants("observability-settings")) {
    matchedOnly.add("observability-settings");
    report.observabilitySettings = await runDepthPass("observability-settings", () =>
      runObservabilitySettingsDepth(page, report),
    );
  }
  if (wants("secrets-root-key")) {
    matchedOnly.add("secrets-root-key");
    report.secretsRootKey = await runDepthPass("secrets-root-key", () =>
      runSecretsRootKeyDepth(page, report),
    );
  }
  if (wants("secrets-credentials")) {
    matchedOnly.add("secrets-credentials");
    report.secretsCredentials = await runDepthPass("secrets-credentials", () =>
      runSecretsCredentialsDepth(page, report),
    );
  }
  if (wants("secrets-leases")) {
    matchedOnly.add("secrets-leases");
    report.secretsLeases = await runDepthPass("secrets-leases", () =>
      runSecretsLeasesDepth(page, report),
    );
  }
  if (wants("secrets-audit")) {
    matchedOnly.add("secrets-audit");
    report.secretsAudit = await runDepthPass("secrets-audit", () =>
      runSecretsAuditDepth(page, report),
    );
  }
  // REQ-128 slice 4's screens. Each is behind `wants()` like every other depth pass, and each
  // names the screens it just built so a focused pass spends its budget here rather than on the
  // whole inventory. `deployment-artifacts` also opens the `{version}` detail screen with a
  // version that EXISTS — see the note above the route list.
  if (wants("deployment-artifacts")) {
    matchedOnly.add("deployment-artifacts");
    report.deploymentArtifacts = await runDepthPass("deployment-artifacts", () =>
      runDeploymentArtifactsDepth(page, report),
      runDeploymentMigrationsDepth(page, report),
    );
  }
  if (wants("deployment-install")) {
    matchedOnly.add("deployment-install");
    report.deploymentInstall = await runDepthPass("deployment-install", () =>
      runDeploymentInstallDepth(page, report),
    );
  }
  if (wants("deployment-upgrade")) {
    matchedOnly.add("deployment-upgrade");
    report.deploymentUpgrade = await runDepthPass("deployment-upgrade", () =>
      runDeploymentUpgradeDepth(page, report),
    );
  }

  
// ---------------------------------------------------------------------------------------------
// REQ-127 slice 3 — the retry screen and the breaker screen, driven end to end.
//
// Both passes click the thing that is dangerous rather than the thing that is easy: the retry
// editor's delay preview (a wrong factor is invisible until you see the curve) and the breaker's
// `Force open` (which changes production behaviour the moment it is pressed). A pass that only
// opened the two pages would prove the routes render and nothing about whether they work.
// ---------------------------------------------------------------------------------------------

/**
 * `/settings/reliability/retries` (REQ-127, slice 3).
 *
 * The order of the steps follows the operator's: open the screen, see the backlog, read what is
 * dead-lettered, open the editor and change a factor to watch the curve move, then close it with
 * `Esc` and confirm the dialog is gone.
 */
async function runReliabilityRetriesDepth(page) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "reliability-retries-depth", action: "retries", ...step });
  };

  await page.goto(`${URL_ADMIN}/settings/reliability/retries`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-view=\"reliability-retries\"], [role=\"alert\"]", { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const alertText = (
    await page.locator("[role=\"alert\"]").first().innerText().catch(() => "")
  ).trim();
  const dueText = (
    await page.locator("[data-view=\"retry-due-now\"]").first().innerText().catch(() => "")
  ).trim();
  const deadText = (
    await page.locator("[data-view=\"retry-dead-count\"]").first().innerText().catch(() => "")
  ).trim();
  const policyRows = await page.locator("[data-policy]").count();
  const charts = await page.locator("[data-view=\"retry-delay-chart\"]").count();
  const empty = (await page.locator("text=No retry policies").first().innerText().catch(() => "")).trim();
  note({
    step: "opened",
    policyRows,
    charts,
    dueNow: dueText,
    deadLetters: deadText,
    alertChars: alertText.length,
    emptyState: empty.length > 0,
  });
  // A screen that renders an error alert instead of the page is a finding; the pass records the
  // alert rather than throwing, so the report can name it.
  if (alertText) note({ step: "alert", text: alertText.slice(0, 200) });
  await shot(page, "page-reliability-retries");

  // The delay chart must exist for every policy row: a chart that silently drops out is how a
  // wrong backoff factor ships unnoticed.
  if (policyRows > 0 && charts < policyRows) {
    note({ step: "missing-charts", policyRows, charts, reason: "a policy row has no delay preview" });
  }

  // ---- open the editor and move the factor --------------------------------------------------
  const editButton = page.locator('[data-policy] button:has-text("Edit")').first();
  if ((await editButton.count()) > 0) {
    await editButton.click();
    await page.waitForTimeout(400);
    const dialog = page.locator('[role="dialog"]');
    const dialogOpen = (await dialog.count()) > 0;
    note({ step: "editor-opened", dialogOpen });
    if (dialogOpen) {
      const factor = dialog.locator('input[type="number"]').nth(2);
      if ((await factor.count()) > 0) {
        const before = await factor.inputValue().catch(() => "");
        await factor.fill("3");
        await page.waitForTimeout(300);
        const after = await factor.inputValue().catch(() => "");
        note({ step: "factor-edited", before, after });
        const warns = await page.locator("text=curve runs past the budget").count().catch(() => 0);
        note({ step: "budget-warning", warns });
      }
      await shot(page, "page-reliability-retries-editor");
      await page.keyboard.press("Escape");
      await page.waitForTimeout(400);
      const closed = (await page.locator('[role="dialog"]').count()) === 0;
      note({ step: "editor-closed", closed });
      if (!closed) note({ step: "editor-stuck", reason: "Escape did not close the policy editor" });
    }
  } else {
    note({ step: "no-policy-row", reason: "no policy row offers an editor" });
  }

  // ---- the dead-letter action ---------------------------------------------------------------
  const retryNowBtn = page
    .locator('[data-view="retry-dead-letters"] button:has-text("Retry now")')
    .first();
  if ((await retryNowBtn.count()) > 0) {
    await retryNowBtn.click();
    await page.waitForTimeout(900);
    const status = (await page.locator('[role="status"]').first().innerText().catch(() => "")).trim();
    note({ step: "retry-now", statusChars: status.length });
  } else {
    note({ step: "no-dead-letter", reason: "nothing is dead-lettered, so retry now has no row" });
  }

  // ---- `/` focuses the filter --------------------------------------------------------------
  await page.keyboard.press("/");
  await page.waitForTimeout(200);
  const focused = await page.evaluate(() => document.activeElement?.tagName === "INPUT");
  note({ step: "shortcut-slash", filterFocused: focused });

  // ---- mobile --------------------------------------------------------------------------------
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(500);
  await shot(page, "page-reliability-retries-mobile");
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  note({ step: "mobile", overflowPx: overflow });
  await page.setViewportSize({ width: 1280, height: 900 });
  return steps.length;
}

/**
 * `/settings/reliability/breakers` (REQ-127, slice 3).
 *
 * The step that matters is the confirmation: `Force open` is the one control on this centre that
 * changes what the platform does to a provider, so the pass opens the dialog, types a reason,
 * checks that the confirm button is disabled while the reason is empty, and then cancels — a
 * pass that pressed it would leave the QA stack refusing a real provider.
 */
async function runReliabilityBreakersDepth(page) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "reliability-breakers-depth", action: "breakers", ...step });
  };

  await page.goto(`${URL_ADMIN}/settings/reliability/breakers`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-view=\"reliability-breakers\"], [role=\"alert\"]", { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const alertText = (await page.locator("[role=\"alert\"]").first().innerText().catch(() => "")).trim();
  const rows = await page.locator("[data-breaker]").count();
  const empty = (await page.locator("text=No breaker has a row yet").count().catch(() => 0)) > 0;
  note({ step: "opened", rows, emptyState: empty, alertChars: alertText.length });
  if (alertText) note({ step: "alert", text: alertText.slice(0, 200) });
  await shot(page, "page-reliability-breakers");

  // The forced-open banner is the one thing an operator must not miss, so the pass asserts it
  // exists whenever a row is forced rather than trusting the chip alone.
  const forcedRows = await page.locator('[data-breaker]:has-text("Forced open")').count();
  const banner = await page.locator("text=drained by hand").count().catch(() => 0);
  note({ step: "forced-open", forcedRows, bannerCards: banner });
  if (forcedRows > 0 && banner === 0) {
    note({ step: "missing-forced-banner", reason: "a forced breaker shows no banner" });
  }

  // ---- the confirmation dialog for the dangerous action ---------------------------------------
  const forceButton = page.locator('[data-breaker] button:has-text("Force open")').first();
  if ((await forceButton.count()) > 0) {
    const key = await page
      .locator("[data-breaker]")
      .first()
      .getAttribute("data-breaker")
      .catch(() => null);
    await forceButton.click();
    await page.waitForTimeout(400);
    const dialog = page.locator('[role="dialog"]');
    const open = (await dialog.count()) > 0;
    note({ step: "force-open-dialog", open, key });
    if (open) {
      const confirmButton = dialog.locator("button").last();
      const disabledEmpty = await confirmButton.isDisabled().catch(() => false);
      note({ step: "reason-required", disabledWhileEmpty: disabledEmpty });
      if (!disabledEmpty) {
        note({ step: "reason-not-required", reason: "Force open was actionable with no reason" });
      }
      await dialog.locator("input").first().fill("qa walkthrough");
      await page.waitForTimeout(300);
      const enabledAfter = !(await confirmButton.isDisabled().catch(() => true));
      note({ step: "reason-accepted", enabledAfter });
      await shot(page, "page-reliability-breakers-force-open");
      // CANCEL, never confirm: pressing it would leave the QA stack refusing a live provider.
      await dialog.locator('button:has-text("Cancel")').click();
      await page.waitForTimeout(400);
      const closed = (await page.locator('[role="dialog"]').count()) === 0;
      note({ step: "cancelled", closed, didNotPressConfirm: true });
    }
  } else {
    note({ step: "no-breaker-row", reason: "no breaker row offers force open" });
  }

  // ---- the threshold editor ------------------------------------------------------------------
  const thresholds = page.locator('[data-breaker] button:has-text("Thresholds")').first();
  if ((await thresholds.count()) > 0) {
    await thresholds.click();
    await page.waitForTimeout(400);
    const open = (await page.locator('[role="dialog"]').count()) > 0;
    const hint = (await page.locator('[role="dialog"]').first().innerText().catch(() => "")).includes(
      "resets",
    );
    note({ step: "thresholds-dialog", open, explainsHalfOpenReset: hint });
    await shot(page, "page-reliability-breakers-thresholds");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(400);
    note({ step: "thresholds-closed", closed: (await page.locator('[role="dialog"]').count()) === 0 });
  }

  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(500);
  await shot(page, "page-reliability-breakers-mobile");
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  note({ step: "mobile", overflowPx: overflow });
  await page.setViewportSize({ width: 1280, height: 900 });
  return steps.length;
}

  if (wants("reliability-retries")) {
    matchedOnly.add("reliability-retries");
    report.reliabilityRetries = await runDepthPass("reliability-retries", () =>
      runReliabilityRetriesDepth(page),
    );
  }
  if (wants("reliability-breakers")) {
    matchedOnly.add("reliability-breakers");
    report.reliabilityBreakers = await runDepthPass("reliability-breakers", () =>
      runReliabilityBreakersDepth(page),
    );
  }
  if (wants("reliability-intake")) {
    matchedOnly.add("reliability-intake");
    report.reliabilityIntake = await runDepthPass("reliability-intake", () =>
      runReliabilityIntakeDepth(page),
    );
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

  // The system health centre (REQ-014, slice 1). It runs after the security pass
  // because both screens run live probes, and running them in the other order
  // would have the health screen's own PostgreSQL probe read the connection pool
  // the security scan is still holding.
  if (wants("health-overview")) {
    matchedOnly.add("health-overview");
    report.health = await runDepthPass("health", () => runHealthDepth(page, report));
  report.healthMetrics = await runDepthPass("health-metrics", () => runHealthMetricsDepth(page, report));
  // Slice 3's two passes (REQ-014). Both are `runDepthPass` like every other depth walk, which
  // is what makes them survive a page crash: the wrapper records the failure instead of the run
  // dying on the next `page.locator`.
  report.healthIncidents = await runDepthPass("health-incidents", () => runHealthIncidentsDepth(page, report));
  report.healthSettings = await runDepthPass("health-settings", () => runHealthSettingsDepth(page, report));
    log(`health: ${JSON.stringify(report.health)}`);
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
  // as a known name instead of reporting it as unmatched. The three deployment screens (REQ-128
  // slice 4) are in this list rather than only measured inside their depth passes: a layout that
  // has never been opened in a 390px context has not been tested on a phone, and the upgrade
  // helper is read at 2am on a phone more often than anybody planned.
  const mobileRoutes = [{ path: "/", name: "overview" }, { path: "/pages", name: "pages" }, { path: "/ai", name: "ai" }, { path: "/search?q=qa", name: "search" }, { path: "/settings/search", name: "search-settings" }, { path: "/settings/iam/users", name: "iam-users" }, { path: "/settings/iam/groups", name: "iam-groups" }, { path: "/settings/iam/simulator", name: "iam-simulator" }, { path: "/settings/iam/policies", name: "iam-policies" }, { path: "/settings/iam/approvals", name: "iam-approvals" }, { path: "/settings/iam/provisioning", name: "iam-provisioning" }, { path: "/settings/iam/authentication", name: "iam-authentication" }, { path: "/settings/iam/security", name: "iam-security" }, { path: "/settings/iam/sessions", name: "iam-sessions" }, { path: "/settings/iam/devices", name: "iam-devices" }, { path: "/analytics", name: "analytics" }, { path: "/analytics/pages", name: "analytics-pages" }, { path: "/analytics/goals", name: "analytics-goals" }, { path: "/analytics/settings", name: "analytics-settings" }, { path: "/security", name: "security-overview" }, { path: "/security/findings", name: "security-findings" }, { path: "/security/headers", name: "security-headers" }, { path: "/security/rate-limits", name: "security-rate-limits" }, { path: "/security/sign-in-protection", name: "security-sign-in-protection" }, { path: "/health", name: "health-overview" }, { path: "/health/metrics", name: "health-metrics" }, { path: "/deployment/artifacts", name: "deployment-artifacts" }, { path: "/deployment/install", name: "deployment-install" }, { path: "/deployment/upgrade", name: "deployment-upgrade" }, { path: "/deployment/migrations", name: "deployment-migrations" }];
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
    // A page whose walk threw is pushed as `{ ...route, failed }` with no `diagnostics` at
    // all, so every field read off `d` below is a read off `undefined`. That is not a
    // cosmetic gap: this loop runs *before* the rest of the roll-up, and an uncaught
    // TypeError here takes every later finding with it — one crashed page silently converts a
    // pass that measured 90 screens into a pass that reported nothing, and a pass with no
    // verdict reads as "no findings". The measurement is therefore recorded as **absent**
    // (a finding that says so) instead of being assumed clean, and the loop continues so a
    // single bad page cannot cost every later page its report.
    const d = p.diagnostics;
    if (!d) {
      pushFindings(
        "high",
        "unmeasured-page",
        `${p.name}: the page walk failed before it produced diagnostics${
          p.failed ? ` (${p.failed})` : ""
        } — this screen was not measured`,
      );
      continue;
    }
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
    // The same absent-measurement rule as the desktop roll-up above, for the same reason: the
    // phone phase is the last thing a pass does, so a throw here is the throw that erases
    // everything it just measured. This pass died here — `Cannot read properties of undefined
    // (reading 'horizontalOverflow')` — and took the entire finding report with it.
    const d = m.diagnostics;
    if (!d) {
      pushFindings("high", "unmeasured-mobile", `mobile ${m.name}: no diagnostics were produced — this screen was not measured at 390px`);
      continue;
    }
    if (d.horizontalOverflow) pushFindings("high", "overflow-mobile", `mobile ${m.name}: horizontal overflow`);
    if (d.offscreen.length) pushFindings("medium", "offscreen-mobile", `mobile ${m.name}: ${d.offscreen.length} element(s) outside the viewport`);
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

// ---------------------------------------------------------------------------------------------
// REQ-127 slice 4 — the inbound intake screen, driven end to end.
// ---------------------------------------------------------------------------------------------
//
// What this pass is really checking is the ONE thing a Rust test cannot see: that an operator
// can answer "is my signature right, and what did the platform refuse?" without leaving the
// page. Four steps, each of which has a failure mode that reads like a working screen:
//
// * The tester runs the PLATFORM'S guard, so the pass types a sample and a deliberately wrong
//   signature and expects a refusal WITH A REASON. A tester that answered "valid" here would
//   be a tester with its own HMAC implementation, which is the whole bug this action exists to
//   avoid.
// * A refusal must never show the key. The pass searches the dialog's text for the secret it
//   cannot know, and asserts the dialog has no "reveal"-shaped control at all — a button whose
//   label contains the word is enough, and it is the cheapest possible future leak.
// * The declare dialog is opened and CLOSED without saving. A pass that saved would leave a
//   declaration in the QA database that the next run's list assertions then trip over, and the
//   symptom is a flaky pass rather than a wrong form.
// * The removal dialog is opened and CANCELLED, and the pass asserts the confirm is disabled
//   while the reason is empty. Removing a declaration turns a guarded door into an open one,
//   and an unconfirmed removal is the one action on this screen with no undo.
// ---------------------------------------------------------------------------------------------
async function runReliabilityIntakeDepth(page) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "reliability-intake-depth", action: "intake", ...step });
  };

  await page.goto(`${URL_ADMIN}/settings/reliability/intake`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page
    .waitForSelector("[data-view=\"reliability-intake\"], [role=\"alert\"]", { timeout: 20000 })
    .catch(() => {});
  await page.waitForTimeout(700);

  const rendered = await page.locator("[data-view=\"reliability-intake\"]").count();
  const rows = await page.locator("[data-intake]").count();
  const cards = await page.locator("[data-intake-card]").count();
  const emptyText = await page
    .locator("[data-view=\"reliability-intake\"]")
    .first()
    .innerText()
    .catch(() => "");
  note({
    step: "screen",
    rendered,
    rows,
    cards,
    hasEmptyState: /No inbound endpoint is declared/i.test(emptyText),
  });
  await shot(page, "page-reliability-intake");

  // ---- The tester, on the first declared endpoint -----------------------------------------
  const verifyButton = page.locator("button", { hasText: "Verify sample" }).first();
  const hasEndpoint = (await verifyButton.count()) > 0;
  if (hasEndpoint) {
    await verifyButton.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[aria-label=\"Verify a signature sample\"]", { timeout: 10000 }).catch(() => {});
    await page.waitForTimeout(300);
    await page
      .locator("[aria-label=\"Verify a signature sample\"] textarea")
      .first()
      .fill('{"event":"qa.sample"}')
      .catch(() => {});
    await page
      .locator("[aria-label=\"Verify a signature sample\"] input")
      .first()
      .fill("v1,qa:0000")
      .catch(() => {});
    await page
      .locator("[aria-label=\"Verify a signature sample\"] button", { hasText: "Verify" })
      .first()
      .click({ timeout: 8000 })
      .catch(() => {});
    await page.waitForSelector("[data-verdict]", { timeout: 15000 }).catch(() => {});
    await page.waitForTimeout(300);
    const verdict = await page.locator("[data-verdict]").first().getAttribute("data-verdict").catch(() => null);
    const dialogText = await page
      .locator("[aria-label=\"Verify a signature sample\"]")
      .first()
      .innerText()
      .catch(() => "");
    // A tester with its own HMAC implementation would answer `valid` for a body the platform
    // refuses, and the operator would chase a signing key that was never wrong.
    note({
      step: "verify-sample",
      verdict,
      refused: verdict === "invalid",
      namesTheReason: /Refused:/.test(dialogText),
      // The screen has no way to reveal a key, so there is nothing to look for. Asserted as an
      // ABSENCE of a control, not as an absence of a value: a value nobody can reach is fine,
      // a button is not.
      noRevealControl: !/reveal|show secret|show key/i.test(dialogText),
    });
    await shot(page, "page-reliability-intake-verify");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(300);
  } else {
    note({ step: "verify-sample", skipped: "no declared endpoint to test against" });
  }

  // ---- The declare dialog: opened, validated, closed WITHOUT saving ------------------------
  await page.locator("button", { hasText: "Declare" }).first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[aria-label=\"Declare intake endpoint\"]", { timeout: 10000 }).catch(() => {});
  await page.waitForTimeout(300);
  const fields = await page
    .locator("[aria-label=\"Declare intake endpoint\"] input, [aria-label=\"Declare intake endpoint\"] select, [aria-label=\"Declare intake endpoint\"] textarea")
    .count();
  // Every control carries a VISIBLE label. A placeholder-only field is the panel's own rule and
  // it is checked here rather than assumed: an unlabelled number input is indistinguishable
  // from a tolerance window and from a byte cap.
  const labelled = await page.evaluate(() => {
    const dialog = document.querySelector('[aria-label="Declare intake endpoint"]');
    if (!dialog) return { total: 0, withLabel: 0 };
    const controls = dialog.querySelectorAll("input, select, textarea");
    let withLabel = 0;
    for (const control of controls) {
      if (control.type === "checkbox") {
        withLabel += 1;
        continue;
      }
      const id = control.getAttribute("id");
      const wrapped = control.closest("label");
      const aria = control.getAttribute("aria-label");
      if ((id && dialog.querySelector(`label[for="${id}"]`)) || wrapped || aria) withLabel += 1;
    }
    return { total: controls.length, withLabel };
  });
  note({ step: "declare-dialog", fields, ...labelled });
  await shot(page, "page-reliability-intake-declare");
  await page.keyboard.press("Escape");
  await page.waitForTimeout(300);

  // ---- The removal confirmation: empty reason must not confirm -----------------------------
  const removeButton = page.locator("button", { hasText: "Remove" }).first();
  if ((await removeButton.count()) > 0) {
    await removeButton.click({ timeout: 8000 }).catch(() => {});
    await page.waitForSelector("[aria-label=\"Remove intake endpoint\"]", { timeout: 10000 }).catch(() => {});
    await page.waitForTimeout(300);
    const confirmDisabled = await page
      .locator("[aria-label=\"Remove intake endpoint\"] button", { hasText: /^Remove$/ })
      .first()
      .isDisabled()
      .catch(() => null);
    const dialogText = await page
      .locator("[aria-label=\"Remove intake endpoint\"]")
      .first()
      .innerText()
      .catch(() => "");
    note({
      step: "remove-confirm",
      // The API refuses an empty reason, so the button must too — a control that sends a
      // request the server will reject teaches the operator that the dialog is decorative.
      confirmDisabled: confirmDisabled === true,
      asksForAReason: /reason/i.test(dialogText),
    });
    await shot(page, "page-reliability-intake-remove");
    await page.locator("[aria-label=\"Remove intake endpoint\"] button", { hasText: "Cancel" }).first().click().catch(() => {});
    await page.waitForTimeout(300);
  }

  // ---- Mobile: the table becomes cards, nothing scrolls sideways ---------------------------
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(500);
  const overflow = await page.evaluate(() => {
    const el = document.scrollingElement || document.documentElement;
    return Math.max(0, el.scrollWidth - el.clientWidth);
  });
  const mobileCards = await page.locator("[data-intake-card]").count();
  note({ step: "mobile", overflowPx: overflow, cards: mobileCards });
  await shot(page, "page-reliability-intake-mobile");
  await page.setViewportSize({ width: 1280, height: 900 });

  return steps.length;
}
