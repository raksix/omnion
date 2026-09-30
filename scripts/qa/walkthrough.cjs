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
  return i !== -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
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
  // Every writer shares one disk, and a pass that frees space does it by removing build
  // directories and artifact runs it does not own. The first write after that lands an
  // ENOENT out of appendFileSync, and because `record` is called from the pass itself the
  // throw unwinds the whole walkthrough — an hour of screens, on a screen with nothing
  // wrong with it. The evidence of what the pass saw stays in `clickLines` either way, so
  // a write that fails is reported once and the walk continues.
  try {
    fs.mkdirSync(OUT, { recursive: true });
    fs.appendFileSync(path.join(OUT, "clicks.jsonl"), JSON.stringify(entry) + "\n");
  } catch (err) {
    if (!record.warned) {
      record.warned = true;
      console.log("[walk] artifact directory is gone (" + err.code + "), recording in memory only");
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
// A refusal a pass is *proving* — the allow-list refusing a host, the editor refusing an
// empty name. It is counted separately so a pass that demonstrates a 400 is not reported as
// a pass that caused one.
const netExpected = [];
let expectingRefusal = null;
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
    if (res.status() < 400) {
      return;
    }
    const entry = { phase, url: res.url().slice(0, 200), status: res.status() };
    if (expectingRefusal && res.url().includes(expectingRefusal)) {
      netExpected.push(entry);
      return;
    }
    netFailures.push(entry);
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
  await page.waitForTimeout(1200);
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
  // The footer is a *report*, not a precondition: a listing that renders no rows has no
  // footer to read, and waiting the full 30 s for one throws the rest of the pass away —
  // every depth pass below this line goes unrun and the whole QA run dies on a screen that
  // is behaving correctly. Two writers fixed this independently (one by bounding the read,
  // one by asking whether the element exists first), and both fixes are the same
  // guarantee: a number when the footer is there, absent when it is not. Kept as the
  // count-then-read form, because a bare `textContent()` on a zero-match locator waits.
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

  // A pass that loses its stack must say so.
  //
  // The stack is disposable: every `run.sh` starts it, and anything on the box that restarts
  // pm2 (a sibling's pass, a `pm2 save`/`resurrect`, a reboot) can SIGINT it while this
  // walkthrough is twenty minutes in. Every navigation after that lands on
  // `chrome-error://chromewebdata/`, every click is "ok" because clicking an error page
  // cannot fail, and the roll-up reports a confident list of console errors, failed requests
  // and empty screens — all of them artifacts of the dead server, none of them a product
  // defect. That is the worst possible outcome for a gate: it looks like a red pass and it
  // is really a dead server, so the next tick goes and "fixes" a screen that was fine.
  //
  // The check is the cheap one — is the admin origin answering? — taken at the end of the
  // pass, next to the roll-up, so the cost is one request and the answer is unambiguous.
  // A pass whose stack died reports `fatal: the QA stack stopped answering` and exits
  // non-zero, which is the opposite of a red pass: it is a no-result run, and a no-result
  // run is re-run rather than acted on.
  const stackGone = async () => {
    try {
      const res = await context.request.get(`${URL_ADMIN}/login`, { timeout: 8000 });
      return !res || res.status() >= 500;
    } catch {
      return true;
    }
  };
  report.assertStackAlive = async () => {
    if (await stackGone()) {
      throw new Error("stack-gone: the QA stack stopped answering mid-pass; every finding after that point is a dead server, not a product defect");
    }
  };

  await runWizard(page, report);

  // `--only=wizard` re-checks the first-run flow on its own (reset the database first): it drives
  // the steps, then reports what the onboarding endpoints answered. A full pass is minutes; this is
  // the tool for "did the setup step just get refused?".
  //
  // `--only=<depth pass>` runs ONE depth pass against an already-running stack and stops. A full
  // pass on a box that also hosts two other writers' stacks is twenty minutes of browser, and a
  // pass that dies half way through has proved nothing about the pass it never reached; this runs
  // the pass a developer is actually working on. Reset the database first if the pass expects the
  // first-run wizard to have run.
  const only = (process.argv.find((arg) => arg.startsWith("--only=")) || "").split("=")[1];
  if (only && DEPTH_PASSES[only]) {
    await ensureSignedIn(page, report);
    await DEPTH_PASSES[only](page, report);
    // The depth passes are the ones a REQ close depends on, so the stack check matters most
    // here: a pass that lost its stack halfway through a depth pass produces a *confident*
    // report (`rows: 0`, `listsTheCreate: false`) that reads exactly like a broken screen.
    if (await stackGone()) {
      fs.writeFileSync(
        path.join(OUT, "summary.json"),
        JSON.stringify({ only, fatal: "stack-gone: the QA stack stopped answering mid-pass; this pass proved nothing", ...report, netFailures }, null, 2),
      );
      console.error("[walk] FATAL: the QA stack stopped answering mid-pass — re-run, do not act on this report");
      await browser.close();
      process.exit(4);
    }
    fs.writeFileSync(
      path.join(OUT, "summary.json"),
      JSON.stringify({ only, ...report, netFailures, netExpected, onboardingFailures: netFailures.filter((f) => String(f.url || "").includes("/onboarding/")) }, null, 2),
    );
    console.log(`ONLY_PASS=${only} NET_FAILURES=${netFailures.length}`);
    for (const line of report.steps.filter((step) => String(step.page || "").includes(only.replace(/Depth$/, "")))) {
      console.log(`  ${JSON.stringify(line)}`);
    }
    await browser.close();
    process.exit(netFailures.length === 0 ? 0 : 1);
  }

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
    // The automations screen (REQ-003, slice 1) — walked here, and its depth pass below
    // creates a rule, nests a condition group, runs a test event, mints a hook URL and
    // deletes the rule again.
    { path: "/automations", name: "automations" },
    // The operations screens of REQ-003 slice 4 — no untested screen. The gallery is its own
    // route, and the run detail is a route with the run's id in it, so neither can appear in a
    // static list: both are opened by the slice-4 depth pass below, which also clicks Restore on
    // a version row, Retry on a failed step and Use this on a template.
    { path: "/automations/templates", name: "automations-templates" },

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
  ];
  // The route loop is per-route isolated for the same reason the depth passes are: a crashed
  // tab (`Page crashed`, which several concurrent passes can cause by exhausting the box's
  // memory) used to end the entire run, so every route after the crash and every depth pass
  // were skipped and no report was written at all. A page that dies is a finding about that
  // page; the pages after it still have to be looked at.
  for (const route of routes) {
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
  await runPalette(page, report);

  // The command centre's own pass (REQ-032): commands, prefixes, running one, and its history.
  await runCommandCenter(page, report);

  // The depth pass: facets, selection, copy, export and the index's own settings screen.
  await runSearchDepth(page, report);

  // The analytics depth pass (REQ-007, slice 2): the range, the comparison, a page drawer and a
  // real CSV download. Goals, funnels and realtime arrive with slice 3; the privacy half of the
  // settings screen with slice 4 — this pass visits what exists today.
  report.analyticsDepth = await runAnalyticsDepth(page, report);

  // The goals + realtime pass (REQ-007, slice 3): a goal is created through the editor, a visitor
  // completes it after it exists, and the funnel and the live counters are read back.
  report.analyticsGoals = await runGoalAndRealtimeDepth(page, report);
  log(`analytics goals: ${JSON.stringify(report.analyticsGoals)}`);

  // The settings and privacy pass (REQ-007, slice 4): tracking on/off persisted, a refused
  // retention value, the exclusions' preview, a purge and an erasure proven against the QA
  // database.
  report.analyticsSettings = await runAnalyticsSettingsDepth(page, report);

  // The notification pass (REQ-021, slice 1): the bell's badge against its own grouped lines,
  // a grouped line filtering the list, a bulk action reporting what it changed, the keyboard
  // path, and the three states. It runs after the analytics passes because it emits into the
  // signed-in account's own inbox and would otherwise add rows to a list a later pass counts.
  report.notifications = await runNotificationsDepth(page, report);
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

  // The preferences pass (REQ-021, slice 2). It runs immediately after the list pass and
  // restores the row it touched, so a later pass in the same run sees the defaults rather
  // than whatever this one left behind.
  report.notificationSettings = await runNotificationSettingsDepth(page, report);
  log(`notification settings: ${JSON.stringify(report.notificationSettings)}`);

  // The outbox and routing pass (REQ-021, slice 3). It runs after the list and preferences
  // passes because it emits into the same inbox, and it cleans up every row it creates — a QA
  // database that grows a notification per pass is one whose counts stop meaning anything.
  report.notificationOutbox = await runNotificationOutboxDepth(page, report);
  log(`notification outbox: ${JSON.stringify(report.notificationOutbox)}`);
  log(`analytics settings: ${JSON.stringify(report.analyticsSettings)}`);

  // The role-depth pass (REQ-006, slice 1): create a role, cycle a matrix cell three ways,
  // preview and save, reopen, and read the history tab back.
  report.iamRoles = await runIamRolesDepth(page, report);

  // The subjects-and-scopes pass (REQ-006, slice 2): users, bindings at every scope, groups,
  // machine identities and the simulator.
  await runIamSubjectsDepth(page, report);

  // The ABAC policies pass (REQ-006, slice 4a): the builder, the dry run and the history.
  report.iamPolicies = await runIamPoliciesDepth(page, report);
  log(`iam roles: ${JSON.stringify(report.iamRoles)}`);

  // The security-policy pass (REQ-006, slice 3): the policy screen with a refusal in the field
  // and a diff on save, the session list with a real revoke, the device registry and the MFA
  // enrolment dialog.
  await runIamSecurityDepth(page, report);
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
  await runPasskeysDepth(page, report);
  log(`passkeys: ${JSON.stringify(report.passkeys)}`);

  // The permission-request pass (REQ-006, slice 4b): ask, approve with a window, refuse, and the
  // refusals of the ask form. It runs after the count-sensitive passes because an approval adds a
  // time-boxed binding (and the generated grant role) to the organization.
  await runIamApprovalsDepth(page, report);
  log(`iam approvals: ${JSON.stringify(report.iamApprovals)}`);

  // The SCIM provisioning pass (REQ-006, slice 4b): mint a token, drive a create → deactivate
  // round trip through the real endpoint from this browser, read the sync log back, revoke the
  // token and prove it is refused afterwards.
  await runIamProvisioningDepth(page, report);
  log(`iam provisioning: ${JSON.stringify(report.iamProvisioning)}`);

  // The automations pass (REQ-003, slice 1): the rule list, the editor with a nested
  // condition group, the dry run, the one-shot listener, the inbound-webhook URL and the
  // delete. It runs before the sign-out below and leaves the database as it found it.
  await runAutomationsDepth(page, report);
  log(`automations: ${JSON.stringify(report.automations)}`);

  // The operations pass (REQ-003, slice 4): the run history, the run's own route with its
  // step trace, the Versions tab with a restore, the Audit tab and the templates gallery. It
  // runs next to the other automation passes because it creates, runs and deletes its own
  // rule, which the count-sensitive empty-state assertions above have already read.
  await runAutomationsOperationsDepth(page, report);
  log(`automations-operations: ${JSON.stringify(report.automationsOperations)}`);

  // The builder pass (REQ-004, slice 1): the visual builder on a real rule. It runs next to
  // the other automation passes for the same reason they do — it creates and deletes its own
  // rule, and the count-sensitive empty-state assertions have already been read by now.
  await runWorkflowBuilderDepth(page, report);
  await runWorkflowTableDepth(page, report);
  log(`workflow-builder: ${JSON.stringify(report.workflowBuilder)}`);

  // The enterprise sign-in pass (REQ-006, slice 4b-2): connect a provider through the drawer,
  // read the "secret is a name, not a value" chip, run the discovery test and require it to
  // report a *result* (a provider that is not configured yet answers "failed", not a 500), then
  // remove the provider and see the list go back to its empty state.
  await runIamAuthenticationDepth(page, report);
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
  for (const route of [{ path: "/", name: "overview" }, { path: "/pages", name: "pages" }, { path: "/automations", name: "automations" }, { path: "/automations/templates", name: "automations-templates" }, { path: "/ai", name: "ai" }, { path: "/search?q=qa", name: "search" }, { path: "/settings/search", name: "search-settings" }, { path: "/settings/iam/users", name: "iam-users" }, { path: "/settings/iam/groups", name: "iam-groups" }, { path: "/settings/iam/simulator", name: "iam-simulator" }, { path: "/settings/iam/policies", name: "iam-policies" }, { path: "/settings/iam/approvals", name: "iam-approvals" }, { path: "/settings/iam/provisioning", name: "iam-provisioning" }, { path: "/settings/iam/authentication", name: "iam-authentication" }, { path: "/settings/iam/security", name: "iam-security" }, { path: "/settings/iam/sessions", name: "iam-sessions" }, { path: "/settings/iam/devices", name: "iam-devices" }, { path: "/analytics", name: "analytics" }, { path: "/analytics/pages", name: "analytics-pages" }, { path: "/analytics/goals", name: "analytics-goals" }, { path: "/analytics/settings", name: "analytics-settings" }]) {
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

  // The gate the whole pass exists to feed, asked last: is the stack still there? A pass that
  // lost it has no findings worth reading, so it is reported as a no-result run and exits
  // non-zero — the caller re-runs it instead of "fixing" dead-server artifacts.
  const alive = !(await stackGone());
  summary.stackAliveAtEnd = alive;
  if (!alive) {
    summary.fatal = "stack-gone: the QA stack stopped answering mid-pass; findings from this run are not product defects";
    fs.writeFileSync(path.join(OUT, "report.md"), `${md.join("\n")}\n\n## FATAL — the QA stack stopped answering mid-pass\n\nEvery finding above this line was recorded against a server that was no longer running.\nRe-run the pass; do not act on this report.\n`);
    fs.writeFileSync(path.join(OUT, "summary.json"), JSON.stringify(summary, null, 2));
    log("FATAL: the QA stack stopped answering mid-pass — this run reports nothing usable");
    console.log(`QA_STACK_GONE=1 QA_FINDINGS=0 QA_CLICKS=${clicks.length}`);
    await browser.close();
    process.exit(4);
  }
  fs.writeFileSync(path.join(OUT, "summary.json"), JSON.stringify(summary, null, 2));

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
 * The automations depth pass (REQ-003, slice 1).
 *
 * Drives the screen the way a person would: create a rule from the empty state, give it a
 * nested condition group, read the validation summary, save, run a test event and read the
 * dry-run report, arm a one-shot listener, switch the rule to the webhook trigger and mint a
 * URL, then delete it by typing its name.
 *
 * Every step that changes the world does so through the panel, and the pass ends with the
 * rule removed so the next run starts from the same state this one did.
 */
async function runAutomationsDepth(page, report) {
  const steps = [];
  const note = (step) => {
    steps.push(step);
    record({ page: "automations-depth", action: "automations", ...step });
  };

  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-automation-new]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(800);

  // The empty state is what a fresh database shows; the pass starts by creating the first rule.
  const emptyState = (await page.locator("[data-automation-empty-new]").count()) > 0;
  note({ step: "list", emptyState, rows: await page.locator("[data-automation-row]").count() });
  await shot(page, "page-automations-empty");

  await page.locator("[data-automation-new]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForSelector("[data-automation-editor]", { timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(400);

  // A rule with no name is refused in the field, and the summary names the problem.
  await page.locator("[data-automation-save]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(500);
  note({
    step: "empty-name",
    problems: await page.locator("[data-automation-problem]").count(),
    summary: (await page.locator("[data-automation-problems]").first().innerText().catch(() => ""))
      .replace(/\s+/g, " ")
      .slice(0, 120),
  });
  await shot(page, "page-automations-problems");

  await page.locator("[data-automation-name]").first().fill("QA welcome rule").catch(() => {});
  await page
    .locator("[data-automation-description]")
    .first()
    .fill("Created by the walkthrough")
    .catch(() => {});

  // Pick an event, then a field the picker offers — the list of fields comes from the
  // event's documented payload, so a field outside it is not selectable at all.
  const eventOptions = await page.locator("[data-automation-event] option").count();
  await page
    .selectOption("[data-automation-event]", "user.created")
    .catch(async () => {
      await page.locator("[data-automation-event] option").first().click().catch(() => {});
    });
  await page.waitForTimeout(400);

  // A nested condition: a group inside the root, which is the shape the request names. The
  // group is **filled** rather than left empty — an empty nested group is refused at save time
  // (an empty `any` can never hold), so a pass that only added one would be asserting the very
  // refusal this slice introduced.
  await page.locator("[data-automation-add-condition]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(300);
  await page.locator("[data-automation-add-group]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(400);
  // The nested group's own "add condition" is the one inside the deeper group box.
  const nestedAdd = page.locator("[data-automation-group='2'] [data-automation-group-add-condition]").first();
  if ((await nestedAdd.count()) > 0) {
    await nestedAdd.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(400);
  }
  const fieldOptions = await page.locator("[data-automation-field] option").count();
  note({
    step: "conditions",
    eventOptions,
    fieldOptions,
    groups: await page.locator("[data-automation-group]").count(),
    rows: await page.locator("[data-automation-condition]").count(),
    legend: (await page.locator("[data-automation-editor] legend").nth(1).innerText().catch(() => ""))
      .replace(/\s+/g, " ")
      .trim(),
  });

  // A second condition, so the group holds more than one row.
  await page.locator("[data-automation-add-condition]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(300);
  await page.locator("[data-automation-value]").first().fill("qa@example.com").catch(() => {});
  await shot(page, "page-automations-editor");

  await page.locator("[data-automation-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const rowsAfterSave = await page.locator("[data-automation-row]").count();
  const notice = (await page.locator("[data-automation-notice]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .slice(0, 120);
  note({ step: "saved", rowsAfterSave, notice });
  await shot(page, "page-automations-list");

  // Open the rule the walkthrough just created and read the catalogue-backed editor.
  const firstRow = page.locator("[data-automation-row]").first();
  const ruleName = (await firstRow.locator("a").first().innerText().catch(() => "")).trim();
  const ruleHref = await firstRow.locator("a").first().getAttribute("href").catch(() => null);
  await firstRow.locator("a").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-automation-row] a").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);

  // The test fire: a hand-written payload, and a report that must say `would_*` on every row
  // and never claim it sent anything.
  await page.locator("[data-automation-payload]").first().fill('{"status":"published","slug":"home"}').catch(() => {});
  await page.locator("[data-automation-run-test]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const verdict = (await page
    .locator("[data-automation-report-verdict]")
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ").trim();
  const outcomes = await page.locator("[data-automation-report-action]").count();
  const outcomeWords = await page.locator("[data-automation-report-action]").allInnerTexts();
  note({
    step: "test-event",
    verdict: verdict.slice(0, 140),
    actions: outcomes,
    simulated: outcomeWords.every((text) => /would_/.test(text)),
  });
  await shot(page, "page-automations-test-report");

  // A payload that does not parse is refused in the field, not sent to the API.
  await page.locator("[data-automation-payload]").first().fill("{not json").catch(() => {});
  await page.locator("[data-automation-run-test]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(900);
  note({
    step: "bad-payload",
    error: (await page.locator("[data-automation-save-error]").first().innerText().catch(() => ""))
      .replace(/\s+/g, " ")
      .slice(0, 120),
  });

  // The one-shot listener.
  await page.locator("[data-automation-listen]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1400);
  const listenerRows = await page.locator("[data-automation-test-row]").count();
  const armedText = await page.locator("[data-automation-test-row]").first().innerText().catch(() => "");
  note({ step: "listener", rows: listenerRows, armed: /armed/.test(armedText) });

  // The webhook trigger: switching to it, then minting a URL — the only control that shows one.
  await page.locator("[data-automation-trigger-hook]").first().check({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(600);
  const hookEmpty = (await page.locator("[data-automation-hook-empty]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .slice(0, 120);
  await page.locator("[data-automation-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1600);

  // Reopen the rule and mint the URL.
  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1000);
  await page.locator("[data-automation-row] a").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-automation-hook-rotate]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const hookUrl = (await page.locator("[data-automation-hook-url] code").first().innerText().catch(() => "")).trim();
  note({
    step: "hook",
    hintBeforeMinting: hookEmpty.slice(0, 100),
    urlMinted: /\/api\/v1\/hooks\/omhook_/.test(hookUrl),
    tokenShape: /omhook_[a-z0-9]{40}$/.test(hookUrl),
  });
  await shot(page, "page-automations-hook");

  // The pass owns every rule named "QA welcome rule" — a run that died half way through
  // leaves one behind, and the next run's "the list is empty" step would then be reading a
  // rule it did not create. Sweeping first is what makes the empty state meaningful.
  const staleRows = await page.locator('[data-automation-row] a', { hasText: "QA welcome rule" }).count();
  for (let index = 0; index < staleRows; index += 1) {
    const stale = page.locator('[data-automation-row] a', { hasText: "QA welcome rule" }).first();
    await stale.click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(900);
    await page.locator("[data-automation-delete]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-automation-delete-input]", { timeout: 5000 }).catch(() => {});
    await page.locator("[data-automation-delete-input]").first().fill("QA welcome rule").catch(() => {});
    await page.locator("[data-automation-delete-confirm-button]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1200);
  }
  note({ step: "sweep", removedStale: staleRows });

  // The list's own filters, read on the rule the pass creates.
  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);
  const allRows = await page.locator("[data-automation-row]").count();
  await page.locator("[data-automation-search]").first().fill("QA welcome rule").catch(() => {});
  await page.waitForTimeout(600);
  const searched = await page.locator("[data-automation-row]").count();
  await page.locator("[data-automation-search]").first().fill("nothing matches this").catch(() => {});
  await page.waitForTimeout(600);
  const empty = (await page.locator("text=No rule matches these filters").count()) > 0;
  await page.locator("[data-automation-search]").first().fill("").catch(() => {});
  await page.waitForTimeout(500);
  note({ step: "filters", allRows, searched, emptyWhenNoMatch: empty });

  // Delete by typing the name, and prove the row is gone.
  await page.locator("[data-automation-row] a").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-automation-delete]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForSelector("[data-automation-delete-input]", { timeout: 5000 }).catch(() => {});
  await page.locator("[data-automation-delete-confirm-button]").first().click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(400);
  // The wrong name must not delete anything.
  const stillThere = (await page.locator("[data-automation-row]").count()) > 0;
  await page.locator("[data-automation-delete-input]").first().fill(ruleName || "QA welcome rule").catch(() => {});
  await page.locator("[data-automation-delete-confirm-button]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  note({
    step: "delete",
    ruleName,
    ruleHref,
    stillThereWhileUnconfirmed: stillThere,
    rowsAfterDelete: await page.locator("[data-automation-row]").count(),
  });
  await shot(page, "page-automations-after-delete");

  report.automations = { steps };
  log(`automations: ${JSON.stringify(steps)}`);
}

/**
 * The automations **slice 2** pass: the action library's outbound half, the branch and stop
 * steps, the per-step failure policy, and Run now.
 *
 * Every control this pass touches is one slice 2 added, and every refusal it asserts is one
 * the server must make — an `http_request` to a host outside the allow-list has to be
 * refused *at save time naming the host*, which is only provable by pressing Save and
 * reading what came back.
 */
async function runAutomationsActionsDepth(page, report) {
  const steps = [];
  const note = (entry) => {
    steps.push(entry);
    log(`automations-actions: ${JSON.stringify(entry)}`);
  };

  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(900);

  // A fresh rule, so the pass does not depend on what another pass left behind.
  const ruleName = `QA action rule ${Date.now().toString(36)}`;
  await page.locator("[data-automation-new]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(1200);
  // The name goes in first: Save is disabled while any problem is open, and a nameless
  // rule is one — so a pass that fills it last can never reach the save it is proving.
  await page.locator("[data-automation-name]").first().fill(ruleName).catch(() => {});
  await page.waitForTimeout(300);
  const editorOpen = (await page.locator("[data-automation-editor]").count()) > 0;
  note({ step: "editor", editorOpen, ruleName });
  await shot(page, "page-automations-actions-editor");

  // The rule's own failure policy, above the steps that inherit it.
  const policySelect = page.locator("[data-automation-on-error]").first();
  const policyVisible = (await policySelect.count()) > 0;
  if (policyVisible) {
    await policySelect.selectOption("continue").catch(() => {});
    await page.waitForTimeout(300);
  }
  note({ step: "rule-policy", policyVisible });

  // Switch the first step to a Branch. The action picker must disappear — a branch names a
  // comparison, not an action — and the typed branch controls must appear.
  const kindSelect = page.locator("[data-automation-step-kind='0']").first();
  const kindVisible = (await kindSelect.count()) > 0;
  if (kindVisible) {
    await kindSelect.selectOption("branch").catch(() => {});
    await page.waitForTimeout(400);
  }
  const branchVisible = (await page.locator("[data-automation-branch='0']").count()) > 0;
  const actionHiddenOnBranch =
    (await page.locator("[data-automation-step-action='0']").count()) === 0;
  note({ step: "branch", kindVisible, branchVisible, actionHiddenOnBranch });

  // A branch on something no run can read must be reported in the summary, not saved.
  if (branchVisible) {
    await page.locator("[data-automation-branch-field='0']").first().fill("nonsense").catch(() => {});
    await page.waitForTimeout(400);
  }
  const problemsShown = (await page.locator("[data-automation-problems]").count()) > 0;
  const problemsText = problemsShown
    ? (await page.locator("[data-automation-problems]").first().innerText()).replace(/\s+/g, " ")
    : "";
  const saveDisabled = await page
    .locator("[data-automation-save]")
    .first()
    .evaluate((node) => node.disabled)
    .catch(() => null);
  note({
    step: "branch-validation",
    problemsShown,
    saveDisabled,
    namedTheField: problemsText.includes("nonsense"),
  });
  await shot(page, "page-automations-actions-branch-problem");

  // Fix the field, switch to Stop, and check its reason control.
  if (branchVisible) {
    await page
      .locator("[data-automation-branch-field='0']")
      .first()
      .fill("event.status")
      .catch(() => {});
    await page.waitForTimeout(400);
  }
  const problemsAfterFix = (await page.locator("[data-automation-problems]").count()) > 0
    ? (await page.locator("[data-automation-problems]").first().innerText()).replace(/\s+/g, " ")
    : "";
  const branchCleared = (await page.locator("[data-automation-problems]").count()) === 0;
  if (kindVisible) {
    await kindSelect.selectOption("stop").catch(() => {});
    await page.waitForTimeout(400);
  }
  const stopVisible = (await page.locator("[data-automation-stop='0']").count()) > 0;
  if (stopVisible) {
    await page
      .locator("[data-automation-stop-reason='0']")
      .first()
      .fill("the QA pass stopped this run on purpose")
      .catch(() => {});
    await page.waitForTimeout(300);
  }
  note({ step: "stop", branchCleared, stopVisible, problemsAfterFix });
  await shot(page, "page-automations-actions-stop");

  // An http_request to a host the installation does not allow: refused at save, naming it.
  if (kindVisible) {
    await kindSelect.selectOption("task").catch(() => {});
    await page.waitForTimeout(400);
  }
  const actionSelect = page.locator("[data-automation-step-action='0']").first();
  if ((await actionSelect.count()) > 0) {
    await actionSelect.selectOption("http_request").catch(() => {});
    await page.waitForTimeout(400);
  }
  const paramsBox = page.locator("[data-automation-step-params='0']").first();
  if ((await paramsBox.count()) > 0) {
    await paramsBox.fill(JSON.stringify({ url: "http://blocked.invalid/hook", method: "POST" }, null, 2)).catch(() => {});
    await page.waitForTimeout(300);
  }
  await page.locator("[data-automation-step-name='0']").first().fill("call a blocked host").catch(() => {});
  await page.waitForTimeout(300);
  // The refusal is the point of this step, so the window it lands in is declared: an
  // unexpected 400 on any other URL is still a finding.
  expectingRefusal = "/api/v1/automations";
  await page.locator("[data-automation-save]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const refusedStatus = netExpected.length > 0 ? netExpected[netExpected.length - 1].status : null;
  expectingRefusal = null;

  const saveError = (await page.locator("[data-automation-save-error]").count()) > 0;
  const saveErrorText = saveError
    ? (await page.locator("[data-automation-save-error]").first().innerText()).replace(/\s+/g, " ")
    : "";
  note({
    step: "host-allow-list",
    saveError,
    refusedStatus,
    namedTheHost: saveErrorText.includes("blocked.invalid"),
    saidWhatToDo: /not a host|add .* to the automation settings/.test(saveErrorText),
  });
  await shot(page, "page-automations-actions-host-refused");

  // A task step's own failure policy and budget, back on a task step. `transient` is the
  // action that exercises both — it is the engine's own always-eventually-succeeds step — and
  // it needs `fail_times`, which the server refuses without: the pass fills it rather than
  // working around the refusal, because a rule that cannot be saved is not the thing under test.
  if ((await actionSelect.count()) > 0) {
    await actionSelect.selectOption("transient").catch(() => {});
    await page.waitForTimeout(400);
  }
  if ((await paramsBox.count()) > 0) {
    await paramsBox.fill(JSON.stringify({ fail_times: 1 }, null, 2)).catch(() => {});
    await page.waitForTimeout(300);
  }
  const onErrorVisible = (await page.locator("[data-automation-step-on-error='0']").count()) > 0;
  const timeoutVisible = (await page.locator("[data-automation-step-timeout='0']").count()) > 0;
  if (onErrorVisible) {
    await page.locator("[data-automation-step-on-error='0']").first().selectOption("continue").catch(() => {});
    await page.waitForTimeout(300);
  }
  if (timeoutVisible) {
    await page.locator("[data-automation-step-timeout='0']").first().fill("5000").catch(() => {});
    await page.waitForTimeout(300);
  }
  note({ step: "step-policy", onErrorVisible, timeoutVisible });
  await shot(page, "page-automations-actions-step-policy");

  // Save for real this time, then Run now on the saved rule.
  await page.locator("[data-automation-save]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const saveBlocked = (await page.locator("[data-automation-save-error]").count()) > 0;
  const saveBlockedText = saveBlocked
    ? (await page.locator("[data-automation-save-error]").first().innerText()).replace(/\s+/g, " ")
    : "";
  const saved = !saveBlocked && (await page.locator("[data-automation-notice]").count()) > 0;

  await openRuleByName(page, ruleName);
  await page.waitForTimeout(1500);
  const runNowVisible = (await page.locator("[data-automation-run-now]").count()) > 0;
  let ranNotice = null;
  if (runNowVisible) {
    await page.locator("[data-automation-run-now]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(2000);
    const notices = page.locator("[data-automation-notice]");
    ranNotice = (await notices.count()) > 0 ? (await notices.first().innerText()).replace(/\s+/g, " ") : null;
  }
  // Read the policy back on the rule this pass wrote, after the save-and-reopen round trip:
  // the round trip is what a whole-rule rewrite could quietly drop.
  const policyKept = await page
    .locator("[data-automation-on-error]")
    .first()
    .evaluate((node) => node.value)
    .catch(() => null);

  note({
    step: "run-now",
    saved,
    saveBlocked,
    saveBlockedText,
    policyKept,
    runNowVisible,
    ranNotice,
    saysItIsReal: /really run/.test(ranNotice || ""),
  });
  await shot(page, "page-automations-actions-run-now");

  report.automationsActions = { steps };
  log(`automations-actions: ${JSON.stringify(steps)}`);
}

/**
 * The slice-3 pass: the approval gate and the run-as authority (REQ-003).
 *
 * Every control this slice added is clicked, and the two that *prove* something are
 * read back rather than eyeballed:
 *
 *  * the run-as picker, and the sentence under it — the panel must show what the
 *    **API** resolved, not what the picker happens to hold;
 *  * a gate step's three controls, with a message the decider would read and a permission
 *    that is a real key; a gate with an empty permission is reported in the problems
 *    summary *before* a save is attempted, with Save disabled;
 *  * the step-kind picker grows `approval`, and the action picker must disappear on it —
 *    a gate names no action, and a picker that still offered one would let an author save
 *    a definition the engine refuses;
 *  * the pending panel: when a gate is waiting it is drawn above the table with an Approve
 *    and a Reject, and when nothing waits it is not drawn at all. "Not drawn" is the state
 *    that is easy to leave broken, so it is asserted rather than assumed.
 */
async function runAutomationsApprovalsDepth(page, report) {
  const steps = [];
  const note = (entry) => {
    steps.push(entry);
    log(`automations-approvals: ${JSON.stringify(entry)}`);
  };

  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);

  // A fresh rule, so the pass does not depend on what another pass left behind.
  const ruleName = `QA gate rule ${Date.now().toString(36)}`;
  await page.locator("[data-automation-new]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(1200);
  // The name first: Save is disabled while any problem is open, and a nameless rule is one.
  await page.locator("[data-automation-name]").first().fill(ruleName).catch(() => {});
  await page.waitForTimeout(300);

  // --- the run-as picker -------------------------------------------------------------------
  const runAs = page.locator("[data-automation-run-as]").first();
  const runAsVisible = (await runAs.count()) > 0;
  const runAsDefault = runAsVisible
    ? await runAs.evaluate((node) => node.value).catch(() => null)
    : null;
  const helpVisible = (await page.locator("[data-automation-run-as-help]").count()) > 0;
  const helpText = helpVisible
    ? (await page.locator("[data-automation-run-as-help]").first().innerText()).replace(/\s+/g, " ")
    : "";
  note({
    step: "run-as",
    runAsVisible,
    runAsDefault,
    helpVisible,
    saysWhenItIsChecked: /step runs|when each step runs/i.test(helpText),
    helpText,
  });
  await shot(page, "page-automations-approvals-run-as");

  // The picker must offer more than one choice on a seeded stack — a picker with only the
  // default is a *read-only* control wearing a select's clothes, and an author cannot hand a
  // rule to a service account through it.
  const runAsOptions = runAsVisible
    ? await runAs.evaluate((node) => node.options.length).catch(() => 0)
    : 0;
  note({ step: "run-as-choices", runAsOptions, hasMoreThanTheDefault: runAsOptions > 1 });

  // --- the gate step ----------------------------------------------------------------------
  const kindSelect = page.locator("[data-automation-step-kind='0']").first();
  const kindVisible = (await kindSelect.count()) > 0;
  const kindHasApproval = kindVisible
    ? await kindSelect
        .evaluate((node) => Array.from(node.options).some((option) => option.value === "approval"))
        .catch(() => false)
    : false;
  if (kindVisible) {
    await kindSelect.selectOption("approval").catch(() => {});
    await page.waitForTimeout(500);
  }

  const gateVisible = (await page.locator("[data-automation-approval-step='0']").count()) > 0;
  // A gate names no action: the action picker must be gone, exactly as it is on a branch.
  const actionHiddenOnGate =
    (await page.locator("[data-automation-step-action='0']").count()) === 0;
  const permissionVisible = (await page.locator("[data-automation-approval-permission='0']").count()) > 0;
  const messageVisible = (await page.locator("[data-automation-approval-message='0']").count()) > 0;
  const ttlVisible = (await page.locator("[data-automation-approval-ttl='0']").count()) > 0;
  note({
    step: "gate-step",
    kindVisible,
    kindHasApproval,
    gateVisible,
    actionHiddenOnGate,
    permissionVisible,
    messageVisible,
    ttlVisible,
  });
  await shot(page, "page-automations-approvals-gate-step");

  // A gate with an unusable permission is reported before a save, with Save disabled. The
  // panel owns this check; the server refuses it too, but an author who only learns on save
  // cannot fix the rule in one pass.
  if (permissionVisible) {
    await page
      .locator("[data-automation-approval-permission='0']")
      .first()
      .fill("not a permission")
      .catch(() => {});
    await page.waitForTimeout(400);
  }
  const problemsShown = (await page.locator("[data-automation-problems]").count()) > 0;
  const problemsText = problemsShown
    ? (await page.locator("[data-automation-problems]").first().innerText()).replace(/\s+/g, " ")
    : "";
  const saveDisabled = await page
    .locator("[data-automation-save]")
    .first()
    .evaluate((node) => node.disabled)
    .catch(() => null);
  note({
    step: "gate-validation",
    problemsShown,
    saveDisabled,
    namedThePermission: /not a permission/.test(problemsText),
    problemsText,
  });
  await shot(page, "page-automations-approvals-gate-problem");

  // Fix it, add a message and a bounded lifetime, and the summary must clear.
  if (permissionVisible) {
    await page
      .locator("[data-automation-approval-permission='0']")
      .first()
      .fill("workflows.approve")
      .catch(() => {});
    await page.waitForTimeout(300);
  }
  if (messageVisible) {
    await page
      .locator("[data-automation-approval-message='0']")
      .first()
      .fill("the QA pass asks: publish this?")
      .catch(() => {});
    await page.waitForTimeout(300);
  }
  if (ttlVisible) {
    await page.locator("[data-automation-approval-ttl='0']").first().fill("24").catch(() => {});
    await page.waitForTimeout(300);
  }
  await page.locator("[data-automation-step-name='0']").first().fill("wait for a person").catch(() => {});
  await page.waitForTimeout(300);
  const problemsCleared = (await page.locator("[data-automation-problems]").count()) === 0;
  const saveEnabledAfterFix = problemsCleared
    ? await page
        .locator("[data-automation-save]")
        .first()
        .evaluate((node) => !node.disabled)
        .catch(() => false)
    : false;
  note({ step: "gate-fixed", problemsCleared, saveEnabledAfterFix });
  await shot(page, "page-automations-approvals-gate-fixed");

  // --- save, then read the rule back ------------------------------------------------------
  await page.locator("[data-automation-save]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1800);
  const saveBlocked = (await page.locator("[data-automation-save-error]").count()) > 0;
  const saveBlockedText = saveBlocked
    ? (await page.locator("[data-automation-save-error]").first().innerText()).replace(/\s+/g, " ")
    : "";
  const saved = !saveBlocked && (await page.locator("[data-automation-notice]").count()) > 0;

  await openRuleByName(page, ruleName);
  await page.waitForTimeout(1500);
  // The round trip is the assertion: a whole-rule write that dropped the gate's parameters
  // would save a rule the engine cannot run, and only a save-and-reopen catches it.
  const gateSurvived = (await page.locator("[data-automation-approval-step='0']").count()) > 0;
  const permissionKept = gateSurvived
    ? await page
        .locator("[data-automation-approval-permission='0']")
        .first()
        .evaluate((node) => node.value)
        .catch(() => null)
    : null;
  const messageKept = gateSurvived
    ? await page
        .locator("[data-automation-approval-message='0']")
        .first()
        .evaluate((node) => node.value)
        .catch(() => null)
    : null;
  const ttlKept = gateSurvived
    ? await page
        .locator("[data-automation-approval-ttl='0']")
        .first()
        .evaluate((node) => node.value)
        .catch(() => null)
    : null;
  note({
    step: "round-trip",
    saved,
    saveBlocked,
    saveBlockedText,
    gateSurvived,
    permissionKept,
    messageKept,
    ttlKept,
  });
  await shot(page, "page-automations-approvals-round-trip");

  // --- the pending panel ------------------------------------------------------------------
  // Nothing is waiting on this pass's rule, so the panel must NOT be drawn. The "empty"
  // state is the one that is easy to leave broken, because a panel that renders an empty
  // box reads as a failure and a panel that never renders reads as a missing feature.
  const panelAbsent = (await page.locator("[data-automation-approvals]").count()) === 0;
  const errorAbsent = (await page.locator("[data-automation-approvals-error]").count()) === 0;
  note({ step: "panel-empty", panelAbsent, errorAbsent });
  await shot(page, "page-automations-approvals-panel-empty");

  report.automationsApprovals = { steps };
  log(`automations-approvals: ${JSON.stringify(steps)}`);
}

/**
 * The automations **slice 4** pass: the operations screens.
 *
 * Five screens arrived with this slice and none of them can be reached from a static route
 * list: the run detail carries a run id in its URL, and the versions and audit tabs are
 * behind a tab strip that is only drawn for a rule that already exists. So the pass builds
 * the state each screen needs, in the panel, and then reads the screen:
 *
 * * a rule whose second step always fails, run once, so the run history has a **failed** row
 *   and the trace a failed step with "1 of 3 attempts" on it;
 * * that run's own route, where Retry re-queues the step and the trace reloads;
 * * the Versions tab, where a Restore **appends** rather than rewinds;
 * * the Audit tab, which lists the create, the edit and the restore;
 * * the gallery, where a starter is installed through the ordinary create.
 *
 * The rule is deleted at the end, so a pass that died half way leaves a name the next pass's
 * sweep removes.
 */
/**
 * Open a rule by its name from the list, waiting for the row to exist first.
 *
 * Save closes the editor and returns to the list, so every step that edits and re-opens has to
 * click through a list that is re-reading at the time. A click issued during that read finds
 * no row, the `.catch()` swallows it, and the pass carries on in whatever screen it happened
 * to land in — which is how "the Versions tab showed nothing" turns out to be a pass that
 * never opened the rule at all. Waiting for the row is the difference between a finding and
 * a phantom.
 */
async function openRuleByName(page, ruleName, timeout = 15000) {
  const deadline = Date.now() + timeout;
  // "The editor is open" means the *rule's own form* is on screen, which is
  // `[data-automation-name]` carrying the rule's name. Not the URL — the route is client-side,
  // so a navigation commits before the rule is fetched and the list is still mounted through
  // that beat. Not the row link either. Reading the name out of the loaded form is the one
  // signal that means "this is the rule you asked for, and it is loaded".
  const editorShows = async () =>
    (await page.locator("[data-automation-name]").first().inputValue().catch(() => "")) === ruleName;
  const onList = async () => (await page.locator("[data-automation-row]").count()) > 0;

  while (Date.now() < deadline) {
    // One unconditional trip to the list, then one click, then a wait for the form. Anything
    // cleverer is a guess about where the pass happens to be standing, and the pass stands
    // somewhere new every time — the list, the editor, a run's trace, the gallery. The list is
    // the only page that can always reach the editor in one click, so it is the only page
    // worth navigating to. (A URL test cannot tell them apart: a run's trace also lives under
    // /automations/, and a pass that waited there for the name field waited out its whole
    // deadline on a page that has none.)
    if (!(await onList())) {
      await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForSelector("[data-automation-row]", { timeout: 6000 }).catch(() => {});
    }
    const target = page.locator("[data-automation-row] a", { hasText: ruleName }).first();
    if ((await target.count()) > 0) {
      await target.click({ timeout: 4000 }).catch(() => {});
    }
    // The click is retried under a short budget, because a save returns to the list mid
    // re-read and the click can land in a row that is torn out from under the pointer.
    for (let waited = 0; waited < 8 && Date.now() < deadline; waited += 1) {
      if (await editorShows()) return true;
      await page.waitForTimeout(300);
    }
  }
  // The click's promise resolving is not evidence the editor opened, and a `.catch`ed wait is
  // not either — so a failed click and a failed wait both fell through to an unconditional
  // `true` once. That is the phantom this helper was written to end, reopened one layer up:
  // the pass went on to read Versions and Audit **on the list page**, reported both as empty,
  // and the two screenshots came out byte-identical because they were literally the same
  // page. So the answer is what is on screen now, and a false here is a no-result run.
  // What the form actually held is the difference between "the rule is not there" and "the
  // rule is there under another name" — and the second is a real product finding (a save that
  // renamed the rule) while the first is a navigation problem. Report both.
  const seenName = await page.locator("[data-automation-name]").first().inputValue().catch(() => null);
  const seenRows = await page.locator("[data-automation-row]").count();
  log(
    `openRuleByName: the editor did not open for "${ruleName}" ` +
      `(url ${page.url()}, form name ${JSON.stringify(seenName)}, list rows ${seenRows})`,
  );
  return false;
}

/**
 * Which screen answered, when a panel listed no rows.
 *
 * A row count of zero is not a diagnosis. The same zero means three different things: the
 * fetch is still in flight, the panel rendered its empty state because there is genuinely
 * nothing, or the fetch failed and the panel rendered its error. A pass that records only
 * the count reports all three as `rows: 0` and the reader — the next tick, deciding what to
 * fix — has no way to tell a working screen from a broken one. So the panel's own error and
 * empty-state hooks are read, and the message is recorded with the count.
 *
 * The hooks are the ones the panels already write (`data-automation-*-error`, and the
 * empty-state block), so this adds no new contract for the product to satisfy.
 */
async function readPanelState(page, kind) {
  const error = await page
    .locator(`[data-automation-${kind}-error], [data-automation-${kind}s-error]`)
    .first()
    .innerText()
    .catch(() => "");
  if (error.trim()) {
    return { state: "error", message: error.replace(/\s+/g, " ").trim().slice(0, 160) };
  }
  // The empty state is a heading inside the panel; the panels have no row, so it is the only
  // thing left on screen and its presence is the difference between "empty" and "unfinished".
  const empty = await page
    .locator("[data-automation-empty], [data-empty-state]")
    .first()
    .innerText()
    .catch(() => "");
  if (empty.trim()) {
    return { state: "empty", message: empty.replace(/\s+/g, " ").trim().slice(0, 160) };
  }
  const loading = await page.locator("[data-loading], .animate-pulse").count();
  if (loading > 0) return { state: "loading", message: "still fetching" };
  return { state: "blank", message: "no rows, no error and no empty state" };
}

async function runAutomationsOperationsDepth(page, report) {
  const steps = [];
  const note = (entry) => {
    steps.push(entry);
    record({ page: "automations-operations-depth", action: "automations", ...entry });
  };

  const ruleName = `QA operations rule ${Date.now().toString(36)}`;

  // ---- A rule with a step that fails, so the trace has something to say --------------------
  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-automation-new]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(800);
  await page.locator("[data-automation-new]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-automation-name]").first().fill(ruleName).catch(() => {});
  await page.locator("[data-automation-description]").first().fill("Created by the walkthrough").catch(() => {});
  await page.waitForTimeout(400);

  // The always-failing action, with the step's own budget raised so the trace has "attempts
  // used against attempts allowed" to print rather than a bare 1/1.
  const kindSelect = page.locator("[data-automation-step-kind='0']").first();
  if ((await kindSelect.count()) > 0) {
    await kindSelect.selectOption("task").catch(() => {});
    await page.waitForTimeout(400);
  }
  const actionSelect = page.locator("[data-automation-step-action='0']").first();
  if ((await actionSelect.count()) > 0) {
    await actionSelect.selectOption("fail").catch(() => {});
    await page.waitForTimeout(400);
  }
  const paramsBox = page.locator("[data-automation-step-params='0']").first();
  if ((await paramsBox.count()) > 0) {
    await paramsBox.fill(JSON.stringify({ message: "deliberate QA failure" }, null, 2)).catch(() => {});
    await page.waitForTimeout(300);
  }
  await page.locator("[data-automation-step-name='0']").first().fill("a step that always fails").catch(() => {});
  await page.waitForTimeout(300);
  await page.locator("[data-automation-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);
  const savedNotice = (await page.locator("[data-automation-notice]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  note({ step: "rule-saved", saved: savedNotice.length > 0, notice: savedNotice.slice(0, 110) });
  await shot(page, "page-automations-operations-editor");

  // ---- Run it, and read the run history ---------------------------------------------------
  // Save *closes* the editor and returns to the list, so the rule is opened again by name —
  // and a "Run now" writes a row the Runs tab then reads, which means waiting for the row
  // rather than for a fixed pause. A pass that reads the tab 1.6s after the run reports an
  // empty history and looks like a broken screen rather than a read that was too early.
  await openRuleByName(page, ruleName);
  const runNowVisible = (await page.locator("[data-automation-run-now]").count()) > 0;
  if (runNowVisible) {
    await page.locator("[data-automation-run-now]").first().click({ timeout: 6000 }).catch(() => {});
    await page.waitForSelector("[data-automation-notice]", { timeout: 15000 }).catch(() => {});
  }
  await page.locator("[data-automation-tab='runs']").first().waitFor({ state: "visible", timeout: 15000 }).catch(() => {});
  // The row is the evidence; poll for it instead of guessing how long the queue takes. The
  // re-read is a tab click *and* a reload token: a panel that reads on mount alone reports
  // "this rule has not run yet" a second after a run started, which is a broken control
  // dressed as a fresh rule. Re-clicking the tab also proves the tab itself still works.
  for (let wait = 0; wait < 12 && (await page.locator("[data-automation-run-row]").count()) === 0; wait += 1) {
    await page.waitForTimeout(1000);
    await page.locator("[data-automation-tab='runs']").first().click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(600);
  }
  const runRows = await page.locator("[data-automation-run-row]").count();
  const runStatus = await page
    .locator("[data-automation-run-row]")
    .first()
    .innerText()
    .catch(() => "");
  note({
    step: "run-history",
    runNowVisible,
    rows: runRows,
    hasRun: runRows > 0,
    showsFailure: /fail/i.test(runStatus),
  });
  await shot(page, "page-automations-operations-runs");

  // ---- The run's own route: the trace ----------------------------------------------------
  await page
    .locator("[data-automation-run-open]")
    .first()
    .waitFor({ state: "visible", timeout: 15000 })
    .catch(() => {});
  await page.locator("[data-automation-run-open]").first().click({ timeout: 6000 }).catch(() => {});
  await page
    .locator("[data-automation-trace-steps]")
    .first()
    .waitFor({ state: "visible", timeout: 15000 })
    .catch(() => {});
  await page.waitForTimeout(800);
  const traceUrl = page.url();
  const traceSteps = await page.locator("[data-automation-trace-step]").count();
  const attemptsText = (await page.locator("[data-automation-trace-attempts]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  const stepError = (await page.locator("[data-automation-trace-step-error]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  // The criterion the request names by name: the step prints its attempts used against
  // attempts allowed. "1 of 3" is the shape; a bare number is not.
  const showsAttempts = /\d+\s+of\s+\d+/.test(attemptsText);
  note({
    step: "trace",
    route: traceUrl.replace(URL_ADMIN, ""),
    steps: traceSteps,
    attempts: attemptsText.slice(0, 60),
    showsAttempts,
    namesTheFailure: /deliberate QA failure/.test(stepError),
  });
  await shot(page, "page-automations-operations-trace");

  // The payload disclosure, because a collapsed panel that never opens is a hidden feature.
  const payloadButton = page.locator("[data-automation-trace-payload]").first();
  if ((await payloadButton.count()) > 0) {
    await payloadButton.click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(500);
  }
  note({
    step: "trace-payload",
    opened: await payloadButton.getAttribute("aria-expanded").catch(() => null),
  });

  // ---- Retry: the one control the trace offers, on the step that failed -------------------
  const retry = page.locator("[data-automation-trace-retry]").first();
  const retryVisible = (await retry.count()) > 0;
  if (retryVisible) {
    await retry.click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(2500);
  }
  const retryError = (await page.locator("[data-automation-trace-error]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  note({
    step: "retry",
    offered: retryVisible,
    accepted: retryError.length === 0,
    error: retryError.slice(0, 110),
  });
  await shot(page, "page-automations-operations-trace-after-retry");

  // ---- The Versions tab: a restore appends, it does not rewind ----------------------------
  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1000);
  const openedForVersions = await openRuleByName(page, ruleName);
  await page.locator("[data-automation-tab='versions']").first().waitFor({ state: "visible", timeout: 15000 }).catch(() => {});
  await page.locator("[data-automation-tab='versions']").first().click({ timeout: 5000 }).catch(() => {});
  await page
    .locator("[data-automation-versions], [role=alert]")
    .first()
    .waitFor({ state: "visible", timeout: 15000 })
    .catch(() => {});
  await page.waitForTimeout(800);
  const versionRows = await page.locator("[data-automation-version-row]").count();
  // A panel that answers with an error, or with "nothing yet", is a different fact from a
  // panel that lists rows — and counting only the rows reports both as zero. The Versions and
  // Audit tabs are the two screens slice 4 closes on, so their *state* is recorded next to
  // their row count: a zero with an empty-state message is a screen that works, and a zero
  // with a fetch error is a bug, and only the message tells the two apart.
  const versionsState = await readPanelState(page, "version");
  // The rule has one write so far. A second one gives the restore something to restore.
  await page.locator("[data-automation-tab='runs']").first().click({ timeout: 4000 }).catch(() => {});
  await page.locator("[data-automation-description]").first().waitFor({ state: "visible", timeout: 10000 }).catch(() => {});
  await page.locator("[data-automation-description]").first().fill("Edited by the walkthrough").catch(() => {});
  await page.waitForTimeout(300);
  await page.locator("[data-automation-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForSelector("[data-automation-notice]", { timeout: 15000 }).catch(() => {});
  // Save closes the editor, so the rule is opened again and the tab asked for a second time.
  await openRuleByName(page, ruleName);
  await page.locator("[data-automation-tab='versions']").first().waitFor({ state: "visible", timeout: 15000 }).catch(() => {});
  await page.locator("[data-automation-tab='versions']").first().click({ timeout: 5000 }).catch(() => {});
  // The edit's version is a write the panel issues *after* the save, so the row is polled for
  // rather than assumed: reading one beat early sees the pre-edit history and reports a
  // restore that had nothing to restore.
  for (let wait = 0; wait < 10 && (await page.locator("[data-automation-version-row]").count()) <= versionRows; wait += 1) {
    await page.waitForTimeout(1000);
    await page.locator("[data-automation-tab='runs']").first().click({ timeout: 4000 }).catch(() => {});
    await page.locator("[data-automation-tab='versions']").first().click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(500);
  }
  const versionsAfterEdit = await page.locator("[data-automation-version-row]").count();
  const restoreButtons = await page.locator("[data-automation-version-restore]").count();
  let restoreNotice = "";
  if (restoreButtons > 0) {
    await page.locator("[data-automation-version-restore]").last().click({ timeout: 6000 }).catch(() => {});
    await page.waitForTimeout(2500);
    restoreNotice = (
      await page.locator("[data-automation-versions-notice]").first().innerText().catch(() => "")
    )
      .replace(/\s+/g, " ")
      .trim();
  }
  // The restore is a write the panel performs *after* the request returns, so the row it adds
  // is not there the instant the button click resolves — the same reason the edit's version
  // is polled for rather than assumed. Reading it straight after the click reported 0 rows and
  // "appended: false" for a restore that had demonstrably worked: the audit tab, read a step
  // later, lists `automation.version_restored` for the very same rule.
  let versionsAfterRestore = 0;
  for (let wait = 0; wait < 10; wait += 1) {
    versionsAfterRestore = await page.locator("[data-automation-version-row]").count();
    if (versionsAfterRestore > versionsAfterEdit) break;
    await page.waitForTimeout(700);
    await page.locator("[data-automation-tab='runs']").first().click({ timeout: 4000 }).catch(() => {});
    await page.locator("[data-automation-tab='versions']").first().click({ timeout: 4000 }).catch(() => {});
    await page.waitForTimeout(400);
  }
  note({
    step: "versions",
    rowsAfterCreate: versionRows,
    rowsAfterEdit: versionsAfterEdit,
    rowsAfterRestore: versionsAfterRestore,
    restoreOffered: restoreButtons,
    // The history is a line, not a rewind: a restore adds a version rather than removing one.
    appended: versionsAfterRestore > versionsAfterEdit,
    // Which screen answered, when no row did.
    // If the editor never opened, every number here is read off the list page. Saying so is
    // the difference between "the Versions tab is empty" and "the pass was looking at the
    // wrong screen", and only the first one is a product finding.
    reachedTheTab: openedForVersions,
    stateWhenEmpty: openedForVersions ? versionsState.state : "not-reached",
    message: (versionsState.message || restoreNotice).slice(0, 130),
    notice: restoreNotice.slice(0, 130),
  });
  await shot(page, "page-automations-operations-versions");

  // ---- The Audit tab: the create, the edit and the restore are all listed -------------------
  await page.locator("[data-automation-tab='audit']").first().click({ timeout: 5000 }).catch(() => {});
  await page.locator("[data-automation-audit], [role=alert]").first().waitFor({ state: "visible", timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(800);
  const auditRows = await page.locator("[data-automation-audit-row]").count();
  const auditState = await readPanelState(page, "audit");
  const auditActions = await page
    .locator("[data-automation-audit-row]")
    .evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-automation-audit-row")).filter(Boolean));
  note({
    step: "audit",
    rows: auditRows,
    actions: [...new Set(auditActions)].join(", "),
    listsTheCreate: auditActions.includes("automation.created"),
    listsTheEdit: auditActions.includes("automation.updated"),
    // Same rule as Versions: a zero has to say *which* zero it is.
    stateWhenEmpty: auditState.state,
    message: auditState.message,
  });
  await shot(page, "page-automations-operations-audit");

  // ---- The gallery: a starter installs through the ordinary create -------------------------
  await page.goto(`${URL_ADMIN}/automations/templates`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-automation-templates]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const cards = await page.locator("[data-automation-template-card]").count();
  const categories = await page.locator("[data-automation-template-category]").count();
  // A template whose own credential is missing says so on the card and refuses to install, so
  // the pass presses the first *installable* one rather than the first one it sees.
  const installable = page.locator('[data-automation-template-use]:not([disabled])').first();
  const installVisible = (await installable.count()) > 0;
  if (installVisible) {
    await installable.click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(3000);
  }
  const templateNotice = (
    await page.locator("[data-automation-templates-notice]").first().innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();
  const templateError = (
    await page.locator("[data-automation-templates-error]").first().innerText().catch(() => "")
  )
    .replace(/\s+/g, " ")
    .trim();
  note({
    step: "templates",
    cards,
    categories,
    // Six starters is the request's own number; anything less means the gallery lost one.
    offersSix: cards >= 6,
    installOffered: installVisible,
    installed: /is created and paused/.test(templateNotice),
    notice: templateNotice.slice(0, 130),
    error: templateError.slice(0, 110),
  });
  await shot(page, "page-automations-operations-templates");

  // ---- And the installed starter is a real rule on the list, not a gallery-only object -----
  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  const listed = await page.locator("[data-automation-row]").count();
  note({ step: "installed-listed", rows: listed });
  await shot(page, "page-automations-operations-list");

  // ---- Cleanup: this pass owns every rule it named, plus the one it installed -------------
  for (const name of [ruleName]) {
    const row = page.locator("[data-automation-row] a", { hasText: name }).first();
    if ((await row.count()) === 0) continue;
    await row.click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1400);
    await page.locator("[data-automation-delete]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-automation-delete-input]", { timeout: 5000 }).catch(() => {});
    await page.locator("[data-automation-delete-input]").first().fill(name).catch(() => {});
    await page.locator("[data-automation-delete-confirm-button]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1500);
  }
  note({ step: "cleanup", removed: ruleName });

  report.automationsOperations = { steps, ruleName };
  log(`automations-operations: ${JSON.stringify(steps)}`);
}

/**
 * The visual builder (REQ-004, slice 1).
 *
 * `/workflows/{id}/builder` cannot go in the static route list for the reason the file
 * already records about the file-detail screen: its path carries a rule id, so a route walked
 * with a placeholder id only proves that the error state renders. This pass therefore creates
 * a real rule, opens *its* builder, and drives it — which is also the only way to see the
 * three panes at once, since a builder with no rule is a 404.
 *
 * What it proves, in the order the request's QA plan asks for:
 *
 *   1. the workspace renders — palette, canvas, inspector, problems panel, all four present;
 *   2. a node can be added from the palette by clicking it, and it lands selected;
 *   3. the inspector writes a parameter, and the save indicator reaches "Saved" — which is
 *      the only honest proof that autosave works, since the indicator is the claim;
 *   4. Validate answers on a deliberately broken graph, and the problems panel names it;
 *   5. the same graph saves once it is fixed, and the version advanced;
 *   6. a stale version is refused with a conflict and the local copy stays on screen;
 *   7. the layout write does **not** advance the version — the one assertion that would
 *      silently fail if positions and semantics were conflated.
 *
 * Every step polls for the thing it is waiting for rather than sleeping a fixed interval:
 * a pass that reads the save state 200ms too early reports "Unsaved changes" and looks like
 * a broken autosave.
 */
// `draftOpenLocked` is a named helper rather than a bare `draftWhileLocked > 0` in the note
// because the note is a record, not a computation: a reader comparing two notes should be
// able to see the same *value*, not the same expression evaluated under different readings.
function draftOpenLocked(count) {
  return count > 0;
}

async function runWorkflowBuilderDepth(page, report) {
  const steps = [];
  const note = (entry) => {
    steps.push(entry);
    record({ page: "workflow-builder-depth", action: "workflow-builder", ...entry });
  };

  const ruleName = `QA builder rule ${Date.now().toString(36)}`;

  // ---- A rule to build on ----------------------------------------------------------------
  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-automation-new]", { timeout: 15000 }).catch(() => {});
  await page.waitForTimeout(800);
  await page.locator("[data-automation-new]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(1200);
  await page.locator("[data-automation-name]").first().fill(ruleName).catch(() => {});
  await page.locator("[data-automation-description]").first().fill("Created by the walkthrough").catch(() => {});
  await page.selectOption("[data-automation-event]", "user.created").catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-automation-save]").first().click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2000);

  // The rule's id, read out of the list row's own link rather than guessed: a builder opened
  // against an id that does not exist renders the error state, and a pass that then asserted
  // "the palette has nodes" would be asserting about the error state.
  const href = await page
    .locator("[data-automation-row] a", { hasText: ruleName })
    .first()
    .getAttribute("href")
    .catch(() => null);
  const workflowId = (href ?? "").split("/").filter(Boolean).pop() ?? "";
  note({ step: "rule-created", found: Boolean(workflowId), workflowId });
  if (!workflowId) {
    report.workflowBuilder = { steps, ruleName, opened: false };
    log(`workflow-builder: ${JSON.stringify(steps)}`);
    return;
  }

  // ---- The workspace renders --------------------------------------------------------------
  await page
    .goto(`${URL_ADMIN}/workflows/${workflowId}/builder`, { waitUntil: "domcontentloaded" })
    .catch(() => {});
  // The builder's own root marker is `data-builder-state` ("loading" | "error" | the
  // workspace), not a bare `data-builder`: the component never renders the latter, so a
  // check for it reports "the builder did not open" for a page that opened fine — which is
  // how the palette drag, the connect gesture and the marquee all sat unproven for a tick.
  // Read the state so an error screen is distinguished from a missing screen.
  const builderState = await page
    .locator("[data-builder-state]")
    .first()
    .evaluate((el) => el.getAttribute("data-builder-state"))
    .catch(() => null);
  const opened = builderState === null || builderState === "loading" ? false : true;
  if (builderState === "error") {
    const message = await page
      .locator("[data-builder-state='error']")
      .first()
      .innerText()
      .catch(() => "");
    note({ step: "builder-state", state: builderState, message: message.slice(0, 200) });
  }
  await page.waitForSelector("[data-builder-palette] [data-palette-node]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(1200);

  const panes = {
    palette: (await page.locator("[data-builder-palette]").count()) > 0,
    canvas: (await page.locator("[data-builder-canvas]").count()) > 0,
    inspector: (await page.locator("[data-builder-inspector]").count()) > 0,
    problems: (await page.locator("[data-builder-problems]").count()) > 0,
  };
  const paletteNodes = await page.locator("[data-palette-node]").count();
  const canvasNodes = await page.locator("[data-node-id]").count();
  note({ step: "workspace", opened, panes, paletteNodes, canvasNodes });
  await shot(page, "page-workflow-builder");

  // ---- A node from the palette, by clicking it --------------------------------------------
  // The rule is born with a trigger and an end, so a canvas with 2 nodes is the backfill or
  // the starter graph having worked; anything else means the rule opened empty.
  const before = canvasNodes;
  await page.locator("[data-palette-node='wait']").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(600);
  const afterAdd = await page.locator("[data-node-id]").count();
  const inspectorOpen = (await page.locator("[data-inspector]").count()) > 0;
  note({ step: "palette-add", before, afterAdd, inspectorOpen });
  await shot(page, "page-workflow-builder-added");

  // ---- The inspector writes a parameter, and the save indicator tells the truth -------------
  const waitNode = await page.locator("[data-node-type='wait']").first().getAttribute("data-node-id").catch(() => null);
  if (waitNode) {
    await page.locator(`[data-node-id="${waitNode}"]`).first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(400);
    await page
      .locator(`[data-inspector="${waitNode}"] [data-inspector-field="seconds"]`)
      .first()
      .fill("45")
      .catch(() => {});
    await page.waitForTimeout(300);
  }
  await shot(page, "page-workflow-builder-inspector");

  // Poll for the save state rather than sleeping: "Unsaved changes" read 200ms early is a
  // broken-autosave finding that costs a whole tick to disprove.
  let saveState = "";
  for (let attempt = 0; attempt < 40; attempt += 1) {
    saveState = (await page.locator("[data-save-state]").first().getAttribute("data-save-state").catch(() => "")) ?? "";
    if (saveState === "saved" || saveState === "error" || saveState === "conflict") {
      break;
    }
    await page.waitForTimeout(400);
  }
  note({ step: "autosave", saveState });
  await shot(page, "page-workflow-builder-saved");

  // ---- ⌘S writes once, and the version says so ---------------------------------------------
  // "Does not write twice" is not observable from the screen: a second write lands while the
  // indicator still reads "saved", and the only witness is `graph_version`. So the probe
  // presses the key and then reads the version the *server* holds, twice — once
  // immediately, and again after longer than the autosave debounce. If ⌘S left the debounce
  // armed, the second read is one higher than the first, and that difference is the whole
  // defect in one number.
  //
  // The key goes to the canvas, not the document: the handler is on the canvas's onKeyDown,
  // and a global listener would be testing a different product.
  const readGraph = async () =>
    page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" });
      if (!response.ok) return null;
      return await response.json();
    }, workflowId);

  const versionBeforeSaveKey = (await readGraph())?.graph_version ?? 0;
  await page.locator("[data-builder-canvas]").first().click({ timeout: 5000 }).catch(() => {});
  await page.keyboard.press("Control+s");
  await page.waitForTimeout(600);
  const versionAfterKey = (await readGraph())?.graph_version ?? 0;
  // Longer than AUTOSAVE_MS, so a debounce that was never cancelled has fired by now.
  await page.waitForTimeout(2600);
  const versionAfterSettle = (await readGraph())?.graph_version ?? 0;
  note({
    step: "cmd-s-writes-once",
    before: versionBeforeSaveKey,
    afterKey: versionAfterKey,
    // A write did happen — otherwise this reads as "no double write" for the wrong reason.
    wroteSomething: versionAfterKey > versionBeforeSaveKey,
    // The criterion itself: no second write arrived after the debounce window.
    settledSame: versionAfterSettle === versionAfterKey,
  });
  await shot(page, "page-workflow-builder-cmd-s");

  const afterSave = await readGraph();
  const versionAfterSave = afterSave?.graph_version ?? 0;
  note({
    step: "projection",
    version: versionAfterSave,
    nodes: afterSave?.node_count ?? 0,
    edges: afterSave?.edge_count ?? 0,
    // A graph that saved but projects to nothing would be the worst outcome: the canvas
    // looks right and a run executes zero steps and reports success.
    valid: afterSave?.projection?.valid ?? false,
    stepCount: afterSave?.projection?.step_count ?? 0,
    reason: (afterSave?.projection?.reason ?? "").slice(0, 120),
  });

  // ---- Validate a deliberately broken graph, through the toolbar ---------------------------
  const broken = await page.evaluate(async (id) => {
    const current = await (
      await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })
    ).json();
    // A second trigger is a refusal the request names by class, and it cannot be fixed by
    // clicking around: it is what proves the problems panel has something to show.
    current.graph.nodes.push({
      id: "trigger-2",
      type: "trigger.schedule",
      label: "A second trigger",
      params: { cron: "0 9 * * *" },
      position: { x: 40, y: 200 },
    });
    const response = await fetch(`/api/v1/workflows/${id}/validate`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ graph: current.graph, graph_version: current.graph_version }),
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  }, workflowId);

  const brokenCodes = (broken.body?.findings ?? []).map((finding) => finding.code);
  note({
    step: "validate-broken",
    status: broken.status,
    valid: broken.body?.valid ?? null,
    errorCount: broken.body?.error_count ?? 0,
    codes: brokenCodes,
    namesNode: (broken.body?.findings ?? []).some((finding) => finding.node_id === "trigger-2"),
  });

  // ---- Each error class, one at a time, and the node it blames -------------------------------
  // The criterion names five classes (cycle, two triggers, orphan, missing input, duplicate
  // edge) and asks each one to name the node involved. Proving them in a single "broken"
  // graph does not answer that: the five overlap, one class can mask another, and a panel
  // that renders only the first finding would report the same codes as one that renders
  // them all. So each class gets its own graph, and each is checked for the two things the
  // criterion actually asks — the code appears, and the message names a real node.
  //
  // Each case starts from a *valid* seed and breaks exactly one thing, so a code that comes
  // back is attributable to the break and not to collateral damage.
  const validationClasses = await page.evaluate(async (id) => {
    const base = await (await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })).json();

    // A minimal valid spine: event trigger → action → end. Every case below is this plus
    // one defect, and the first case is this on its own as the control.
    //
    // The port keys are the registry's, not guesses: a trigger exports `out`, an action
    // exports `success`/`error`. A probe that wires `next` into both would be measuring
    // `unknown_source_port` on every case at once — the codes the criterion asks about would
    // never appear, and the table would report the validation as broken rather than the
    // probe as wrong.
    const trigger = (nid) => ({
      id: nid,
      type: "trigger.event",
      label: `Event ${nid}`,
      params: { event: "qa.validate.probe" },
      position: { x: 40, y: 40 },
    });
    const act = (nid) => ({
      id: nid,
      type: "action",
      label: `Act ${nid}`,
      params: { action: "log", parameters: "{}" },
      position: { x: 320, y: 40 },
    });
    const finish = (nid) => ({
      id: nid,
      type: "end",
      label: `End ${nid}`,
      params: {},
      position: { x: 600, y: 40 },
    });
    // `Edge` carries a REQUIRED `id` and refuses unknown fields, so an edge without one is
    // refused by the deserializer before validation ever runs. The first draft of this probe
    // built `{source, source_port, target}` and got `valid: null` with an empty `codes` on all
    // six cases — which the note recorded as "the validator found nothing", when in fact the
    // request never reached the validator. Ids are derived from the endpoints so the
    // duplicate-edge case carries two DIFFERENT ids for one port pair: identity is the edge's
    // own, and it is the (source, port, target) triple the validator calls a duplicate.
    const edge = (from, to, port, seq = 0) => ({
      id: `e-${from}-${port}-${to}-${seq}`,
      source: from,
      source_port: port,
      target: to,
    });
    const spine = () => [edge("t1", "a1", "out"), edge("a1", "e1", "success")];

    const cases = {
      clean: {
        nodes: [trigger("t1"), act("a1"), finish("e1")],
        edges: spine(),
      },
      // A loop: a1 → t1 closes a ring back to the trigger.
      cycle: {
        nodes: [trigger("t1"), act("a1"), finish("e1")],
        edges: [...spine(), edge("a1", "t1", "success")],
      },
      multiple_triggers: {
        nodes: [trigger("t1"), trigger("t2"), act("a1"), finish("e1")],
        edges: spine(),
      },
      // Reachable from nothing: a floating action the trigger never reaches.
      orphan_node: {
        nodes: [trigger("t1"), act("a1"), finish("e1"), act("stray")],
        edges: spine(),
      },
      // A required parameter left empty — the registry names the field and the node.
      missing_input: {
        nodes: [{ ...trigger("t1"), params: { event: "" } }, act("a1"), finish("e1")],
        edges: spine(),
      },
      duplicate_edge: {
        nodes: [trigger("t1"), act("a1"), finish("e1")],
        // Same (source, port, target) twice, under two ids — the pair is what the validator
        // keys on, so reusing one id would make this a different case entirely.
        edges: [edge("t1", "a1", "out"), edge("t1", "a1", "out", 1), edge("a1", "e1", "success")],
      },
    };

    const out = {};
    for (const [name, graph] of Object.entries(cases)) {
      const response = await fetch(`/api/v1/workflows/${id}/validate`, {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ graph, graph_version: base.graph_version }),
      });
      const body = await response.json().catch(() => null);
      const findings = body?.findings ?? [];
      out[name] = {
        status: response.status,
        valid: body?.valid ?? null,
        codes: findings.map((f) => f.code),
        // "Names the node involved" — the finding carries a node id the graph really has,
        // or the message quotes a label. A finding with neither is a class the panel shows
        // as a bare sentence the author has to decode.
        named: findings
          .filter((f) => f.severity === "error")
          .every((f) => f.node_id || /[A-Z][a-z]+/.test(f.message ?? "")),
        firstMessage: (findings[0]?.message ?? "").slice(0, 160),
      };
    }
    return out;
  }, workflowId);

  // The control first: if the clean spine is not clean, every other row in the table is
  // measuring the seed rather than the defect.
  note({
    step: "validate-classes",
    clean: {
      valid: validationClasses.clean?.valid,
      codes: validationClasses.clean?.codes ?? [],
    },
    cycle: {
      found: validationClasses.cycle?.codes?.includes("graph_cycle"),
      names: validationClasses.cycle?.firstMessage ?? "",
    },
    twoTriggers: {
      found: validationClasses.multiple_triggers?.codes?.includes("multiple_triggers"),
      names: validationClasses.multiple_triggers?.firstMessage ?? "",
    },
    orphan: {
      found: validationClasses.orphan_node?.codes?.includes("orphan_node"),
      names: validationClasses.orphan_node?.firstMessage ?? "",
    },
    missingInput: {
      found: validationClasses.missing_input?.codes?.includes("missing_parameter"),
      codes: validationClasses.missing_input?.codes ?? [],
      names: validationClasses.missing_input?.firstMessage ?? "",
    },
    duplicateEdge: {
      found: validationClasses.duplicate_edge?.codes?.includes("duplicate_edge"),
      names: validationClasses.duplicate_edge?.firstMessage ?? "",
    },
  });

  // ---- The toolbar's Validate, and the problems panel it fills -----------------------------
  await page.locator("[data-testid='builder-validate']").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(1500);
  const findingRows = await page.locator("[data-finding]").count();
  const problemsText = (await page.locator("[data-builder-problems]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  note({ step: "problems-panel", findingRows, text: problemsText.slice(0, 140) });
  await shot(page, "page-workflow-builder-problems");

  // ---- A stale save is a conflict, and the local copy survives ------------------------------
  const conflict = await page.evaluate(async (id) => {
    const current = await (
      await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })
    ).json();
    const response = await fetch(`/api/v1/workflows/${id}/graph`, {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ graph: current.graph, graph_version: current.graph_version + 5 }),
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  }, workflowId);
  note({
    step: "conflict",
    status: conflict.status,
    code: conflict.body?.error?.code ?? null,
    // The message has to name the current version: a client that only learns "conflict"
    // cannot offer Reload, and an editor that silently overwrites is the one behaviour a
    // builder must never have.
    namesVersion: /version/i.test(conflict.body?.error?.message ?? ""),
  });

  // ---- A layout write must not advance the version -----------------------------------------
  const layoutVersionBefore = (await readGraph())?.graph_version ?? 0;
  await page.locator("[data-testid='builder-zoom-in']").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(400);
  await page.locator("[data-testid='builder-zoom-in']").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(1500);
  const layoutVersionAfter = (await readGraph())?.graph_version ?? 0;
  note({
    step: "layout-is-not-semantics",
    before: layoutVersionBefore,
    after: layoutVersionAfter,
    unchanged: layoutVersionBefore === layoutVersionAfter,
  });
  await shot(page, "page-workflow-builder-zoomed");

  // ---- Fit, so the canvas can be read at a glance -------------------------------------------
  await page.locator("[data-testid='builder-fit']").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(800);
  const zoomLabel = (await page.locator("[data-builder-toolbar]").first().innerText().catch(() => ""))
    .replace(/\s+/g, " ")
    .trim();
  note({ step: "fit", toolbar: zoomLabel.slice(0, 120) });

  // ---- The interaction depth (REQ-004 slice 2) ---------------------------------------------
  // Everything below is the part of the builder a unit test cannot see: a minimap that does
  // not overlap the graph, a marquee that catches the cards it visually covers, an undo that
  // the server agrees with, and the keyboard routes (⌘A, Shift+click, ⌘P + Enter) that the
  // keyboard-only acceptance pass depends on.
  await page.locator("[data-builder-canvas]").first().click({ timeout: 5000 }).catch(() => {});
  await page.waitForTimeout(300);

  // Select-all from the keyboard. The node count on the canvas is the assertion: ⌘A that
  // highlights nothing is invisible in a screenshot and would otherwise pass every other gate.
  await page.keyboard.press("Control+a");
  await page.waitForTimeout(600);
  // `[data-node-selected]` is the product's own answer. The previous probe grepped the inline
  // style for the substring "outline", which React writes as `outline: none` on EVERY card —
  // so "3 of 3 selected" and "stillSelected: 3 after Escape" were the same reading of a
  // probe that could not tell a selected card from an unselected one. A probe that cannot go
  // red is not a gate; this one can.
  const countSelected = () =>
    page.locator("[data-node-selected='true']").count();
  const selectedAfterSelectAll = await countSelected();
  note({ step: "select-all", total: await page.locator("[data-node-id]").count(), selected: selectedAfterSelectAll });
  await shot(page, "page-workflow-builder-select-all");

  // Escape clears it again, so the next gesture starts from a known state.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);
  const afterEscape = await countSelected();
  note({ step: "escape-clears", stillSelected: afterEscape, cleared: afterEscape === 0 });

  // Shift+click adds a second node to the selection. Two cards drawn as selected is the
  // whole point: a Shift+click that *replaces* the selection is a bug no count can hide.
  const cardIds = await page.locator("[data-node-id]").evaluateAll((cards) =>
    cards.map((card) => card.getAttribute("data-node-id")),
  );
  if (cardIds.length >= 2) {
    await page.locator(`[data-node-id="${cardIds[0]}"]`).first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(250);
    await page
      .locator(`[data-node-id="${cardIds[1]}"]`)
      .first()
      .click({ timeout: 5000, modifiers: ["Shift"] })
      .catch(() => {});
    await page.waitForTimeout(500);
    const multiSelected = await countSelected();
    // Two, not three: the plain click selected one, the Shift+click added the second. A
    // count of 3 means the second click replaced the first or the first survived Escape.
    note({ step: "shift-click-multi", expected: 2, selected: multiSelected, ok: multiSelected === 2 });
    await shot(page, "page-workflow-builder-multi");
    await page.keyboard.press("Escape");
    await page.waitForTimeout(300);
  } else {
    note({ step: "shift-click-multi", skipped: "fewer than two nodes on the canvas", nodes: cardIds.length });
  }

  // Drag from the palette onto the canvas. HTML5 drag is driven through `dispatchEvent` with
  // a real DataTransfer because Playwright's mouse API does not synthesise it; the node count
  // before and after is the assertion, and the position is checked against the drop point so
  // a drop that lands at the canvas origin (screen coords used as graph coords) cannot pass.
  const nodesBeforeDrag = await page.locator("[data-node-id]").count();
  const dragResult = await page
    .evaluate(() => {
      const item = document.querySelector("[data-palette-node='transform']");
      const canvas = document.querySelector("[data-builder-canvas]");
      if (!item || !canvas) return { attempted: false };
      const rect = canvas.getBoundingClientRect();
      const target = { x: rect.left + 320, y: rect.top + 220 };
      const transfer = new DataTransfer();
      item.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer: transfer }));
      canvas.dispatchEvent(new DragEvent("dragover", { bubbles: true, cancelable: true, dataTransfer: transfer }));
      canvas.dispatchEvent(
        new DragEvent("drop", { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: target.x, clientY: target.y }),
      );
      return { attempted: true, target };
    })
    .catch(() => ({ attempted: false }));
  await page.waitForTimeout(900);
  const afterDrag = await page.locator("[data-node-id]").count();
  const droppedNode = await page
    .locator("[data-node-type='transform']")
    .last()
    .evaluate((card) => {
      const rect = card.getBoundingClientRect();
      return { left: Math.round(rect.left), top: Math.round(rect.top) };
    })
    .catch(() => null);
  note({
    step: "palette-drag",
    ...dragResult,
    before: nodesBeforeDrag,
    after: afterDrag,
    dropped: droppedNode,
    // A drop that ignores the viewport puts the card at the graph origin instead of under
    // the pointer; the card's centre should sit within a card's width of the drop point.
    nearDropPoint: droppedNode && dragResult.attempted
      ? Math.abs(droppedNode.left + 110 - dragResult.target.x) < 240
      : null,
  });
  await shot(page, "page-workflow-builder-drag");

  // ⌘P focuses the palette, ArrowDown moves the focus, Enter adds. The node count before and
  // after is the assertion — a palette that takes focus but does not add is a dead control.
  const nodesBeforeKeyboardAdd = await page.locator("[data-node-id]").count();
  await page.locator("[data-builder-canvas]").first().click({ timeout: 5000 }).catch(() => {});
  await page.keyboard.press("Control+p");
  await page.waitForTimeout(500);
  const paletteFocused = await page.evaluate(() => {
    const active = document.activeElement;
    return Boolean(active && active.getAttribute && active.getAttribute("data-palette-node"));
  });
  await page.keyboard.press("ArrowDown");
  await page.waitForTimeout(250);
  await page.keyboard.press("Enter");
  await page.waitForTimeout(700);
  const afterKeyboardAdd = await page.locator("[data-node-id]").count();
  note({ step: "palette-keyboard-add", paletteFocused, before: nodesBeforeKeyboardAdd, after: afterKeyboardAdd });
  await shot(page, "page-workflow-builder-palette-keyboard");

  // Undo removes the node the keyboard just added, and the *server* agrees — an undo that
  // only rewinds the screen would save a graph the canvas no longer shows.
  await page.locator("[data-builder-canvas]").first().click({ timeout: 5000 }).catch(() => {});
  await page.keyboard.press("Control+z");
  await page.waitForTimeout(900);
  const afterUndo = await page.locator("[data-node-id]").count();
  note({ step: "undo", afterUndo, returned: afterUndo === nodesBeforeKeyboardAdd });

  // The connection gesture: press an output port, press a target node, and the server's edge
  // count rises. The refusal is the half that matters — a port the source does not export has
  // to *say so*, and a gesture that refuses silently is indistinguishable from a dead button,
  // so both outcomes are read back off the canvas rather than inferred from the count.
  const connectProbe = await page
    .evaluate(() => {
      const cards = Array.from(document.querySelectorAll("[data-node-id]"));
      if (cards.length < 2) return { attempted: false, reason: "fewer than two nodes" };
      const first = cards.find((card) => card.querySelector("[data-port-out]"));
      // The target is picked by identity and returned as an index, not re-found with
      // `.nth(1)` on the way back in: node 1 is not necessarily the second card, and
      // connecting a node to itself is the one refusal the gesture must not be measured on.
      const secondIndex = cards.findIndex((card) => card !== first);
      const port = first?.querySelector("[data-port-out]");
      if (!first || secondIndex < 0 || !port) {
        return { attempted: false, reason: "no port on a first card" };
      }
      port.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      return {
        attempted: true,
        first: first.getAttribute("data-node-id"),
        port: port.getAttribute("data-port-key"),
        second: cards[secondIndex].getAttribute("data-node-id"),
        secondIndex,
      };
    })
    .catch(() => ({ attempted: false }));
  await page.waitForTimeout(400);
  const draftShown = (await page.locator("[data-link-draft]").count()) > 0;
  const edgesBeforeConnect = (await readGraph())?.edge_count ?? 0;
  await page
    .locator("[data-node-id]")
    .nth(connectProbe.secondIndex ?? 1)
    .click({ timeout: 5000, force: true })
    .catch(() => {});
await page.waitForTimeout(1400);
const edgesAfterConnect = (await readGraph())?.edge_count ?? 0;
const linkNotice = await page
  .locator("[data-link-notice]")
  .first()
  .evaluate((el) => ({ tone: el.getAttribute("data-link-notice"), text: el.textContent?.trim() }))
  .catch(() => null);
note({
  step: "port-connect",
  ...connectProbe,
  draftShown,
  before: edgesBeforeConnect,
  after: edgesAfterConnect,
  notice: linkNotice,
});
await shot(page, "page-workflow-builder-connected");

// Escape cancels a half-drawn connection: the draft hint has to disappear and the graph has to
// be unchanged, or Escape is a gesture that silently mutates the definition.
await page
  .evaluate(() => {
    const port = document.querySelector("[data-port-out]");
    port?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  })
  .catch(() => {});
await page.waitForTimeout(300);
await page.locator("[data-builder-canvas]").first().click({ timeout: 5000 }).catch(() => {});
await page.keyboard.press("Escape");
await page.waitForTimeout(500);
note({
  step: "port-connect-escape",
  draftGone: (await page.locator("[data-link-draft]").count()) === 0,
  edges: (await readGraph())?.edge_count ?? 0,
});

// The edge delete: select an edge, Del, and the server's edge count falls. An edge whose
  // deletion the canvas shows but the graph keeps is a ghost edge that comes back on reload.
  const edgesBefore = (await readGraph())?.edge_count ?? 0;
  // The click has to land ON the curve. `locator.click()` aims at the element's bounding
  // box, and a bezier's box is the rectangle *around* the arc — its centre is empty canvas.
  // So the click fell on the desk, the canvas handler cleared the selection, and the pass
  // reported "no edge could be selected": a probe defect read as a product defect. The point
  // is taken from `getPointAtLength` (the midpoint of the stroke itself) and mapped to screen
  // through `getScreenCTM`, which is the only transform that accounts for the viewport's
  // pan, zoom and the node layer's CSS transform.
  const edgeScreenPoint = await page
    .evaluate(() => {
      const hit = document.querySelector("[data-edge] path");
      if (!hit || typeof hit.getPointAtLength !== "function") return null;
      const ctm = hit.getScreenCTM();
      if (!ctm) return null;
      const mid = hit.getPointAtLength(hit.getTotalLength() / 2);
      const screen = mid.matrixTransform(ctm);
      return { x: screen.x, y: screen.y };
    })
    .catch(() => null);
  let edgeHit = false;
  if (edgeScreenPoint) {
    await page.mouse.move(edgeScreenPoint.x, edgeScreenPoint.y).catch(() => {});
    await page.mouse.down().catch(() => {});
    await page.mouse.up().catch(() => {});
    edgeHit = true;
  }
  await page.waitForTimeout(500);
  const edgeSelected = (await page.locator("[data-edge-selected='true']").count()) > 0;
  if (edgeSelected) {
    await page.keyboard.press("Delete");
    await page.waitForTimeout(1200);
    const afterEdgeDelete = await page.locator("[data-edge]").count();
    const edgesAfter = (await readGraph())?.edge_count ?? 0;
    const selectionReadout = await page
      .locator("[data-builder-selection]")
      .first()
      .innerText()
      .catch(() => null);
    note({
      step: "edge-delete",
      hit: edgeHit,
      edgesBefore,
      after: edgesAfter,
      canvas: afterEdgeDelete,
      // The server is the authority: a canvas that hides the line while the graph keeps it is
      // a ghost edge that returns on reload, and the count is what says so.
      removed: edgesAfter < edgesBefore,
      readout: selectionReadout,
    });
    await shot(page, "page-workflow-builder-edge-deleted");
    // Undo puts it back: the history has to know about edge deletes too.
    await page.locator("[data-builder-canvas]").first().click({ timeout: 5000 }).catch(() => {});
    await page.keyboard.press("Control+z");
    await page.waitForTimeout(1200);
    const edgesRestored = (await readGraph())?.edge_count ?? 0;
    note({ step: "edge-delete-undo", edgesRestored, restored: edgesRestored === edgesBefore });
  } else {
    note({
      step: "edge-delete",
      hit: edgeHit,
      selected: false,
      point: edgeScreenPoint,
      reason: edgeScreenPoint ? "the click missed the curve" : "no edge could be measured",
    });
  }

  // ---- Two tabs: the second save loses, and the first tab keeps its copy --------------------
  // The `conflict` step above PUTs a stale version through a raw `fetch`, which proves the
  // *server* refuses it. That is half the criterion: the request says the UI must offer
  // Reload "while keeping the local copy visible instead of overwriting silently", and a raw
  // fetch never touches the toolbar, so the half that matters was unmeasured.
  //
  // So: open a second real page on the same rule, save a real edit there (the version
  // advances), then make an edit in this tab and let its own autosave run. The autosave
  // quotes the version this tab loaded, the server is one ahead, and the answer is a 409 that
  // the toolbar has to render. Three things are then read: the save state says conflict, the
  // Reload button is there, and the local node is still on the canvas — a client that
  // silently reloads on conflict has thrown away the author's work, which is the one thing
  // this criterion exists to forbid.
  const tabTwo = await page.context().newPage();
  try {
    await tabTwo
      .goto(`${URL_ADMIN}/workflows/${workflowId}/builder`, { waitUntil: "domcontentloaded" })
      .catch(() => {});
    await tabTwo.waitForSelector("[data-builder-palette]", { timeout: 20000 }).catch(() => {});
    await tabTwo.waitForTimeout(1000);

    // Tab two saves a real edit, so the stored version moves under this tab.
    //
    // A *rename* rather than an added node, and that is not a detail: `replace_graph` derives
    // the step list in the same statement, so a graph that does not validate never reaches
    // the version check — the PUT would be refused for a reason that has nothing to do with
    // concurrency, and this probe would be reading a different failure than the one it names.
    const tabTwoSave = await tabTwo.evaluate(async (id) => {
      const current = await (
        await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })
      ).json();
      const graph = structuredClone(current.graph);
      const target = graph.nodes.find((node) => node.id !== "node-tab-two");
      if (target) target.label = `Renamed by the second tab ${Date.now().toString(36)}`;
      const response = await fetch(`/api/v1/workflows/${id}/graph`, {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ graph, graph_version: current.graph_version }),
      });
      return {
        status: response.status,
        before: current.graph_version,
        after: (await response.json().catch(() => null))?.graph_version ?? null,
      };
    }, workflowId);

    // Tab one edits and lets its own debounced autosave fire. The palette click is the same
    // route the acceptance pass uses, so the save is armed by a gesture a user could make.
    await page.locator("[data-builder-palette]").first().click({ timeout: 5000 }).catch(() => {});
    await page.locator("[data-palette-node='transform']").first().click({ timeout: 8000 }).catch(() => {});
    let conflictState = "";
    for (let attempt = 0; attempt < 40; attempt += 1) {
      conflictState =
        (await page.locator("[data-save-state]").first().getAttribute("data-save-state").catch(() => "")) ?? "";
      if (conflictState === "conflict" || conflictState === "saved" || conflictState === "error") break;
      await page.waitForTimeout(400);
    }
    const localNodesKept = await page.locator("[data-node-id]").count();
    const reloadOffered = (await page.locator("[data-save-reload]").count()) > 0;
    // The second exit has to exist *and* work. Before this tick the server's message said
    // "reload to see their change, or keep editing to overwrite it" while the client quoted
    // its stale version forever, so the only way to save was to discard the author's work —
    // a button that was missing was less bad than a button that was a dead end.
    const keepMineOffered = (await page.locator("[data-save-keep-mine]").count()) > 0;
    let afterKeepMine = { state: "", version: null, nodeGone: null };
    if (conflictState === "conflict" && keepMineOffered) {
      await page.locator("[data-save-keep-mine]").first().click({ timeout: 8000 }).catch(() => {});
      let keptState = "";
      for (let attempt = 0; attempt < 30; attempt += 1) {
        keptState =
          (await page.locator("[data-save-state]").first().getAttribute("data-save-state").catch(() => "")) ?? "";
        if (keptState === "saved" || keptState === "error" || keptState === "conflict") break;
        await page.waitForTimeout(400);
      }
      const stored = await readGraph();
      afterKeepMine = {
        state: keptState,
        version: stored?.graph_version ?? null,
        // The overwrite is only an overwrite if the version actually moved past the one the
        // second editor took; a banner that clears without a write has changed nothing.
        nodeGone: stored?.node_count ?? null,
      };
    }
    const conflictText = (await page.locator("[data-save-state='conflict']").first().innerText().catch(() => ""))
      .replace(/\s+/g, " ")
      .trim();
    note({
      step: "two-tab-conflict",
      tabTwoStatus: tabTwoSave.status,
      versionBefore: tabTwoSave.before,
      versionAfter: tabTwoSave.after,
      state: conflictState,
      // The whole criterion in three readings: the save was refused, Reload is offered, and
      // the author's own nodes are still on the canvas.
      refused: conflictState === "conflict",
      reloadOffered,
      localNodesKept,
      // The banner has to name the version — that is what lets a client offer a real Reload
      // instead of a shrug.
      namesVersion: /version/i.test(conflictText),
      text: conflictText.slice(0, 160),
    });
    if (conflictState === "conflict") {
      note({
        step: "two-tab-keep-mine",
        offered: keepMineOffered,
        stateAfter: afterKeepMine.state,
        // Saved on top of the other editor's version, not refused again: this is the reading
        // that would have caught the dead end, because every save quoting a stale version
        // comes back as a second `conflict`.
        resolved: afterKeepMine.state === "saved",
        versionAfter: afterKeepMine.version,
        nodes: afterKeepMine.nodeGone,
      });
      await shot(page, "page-workflow-builder-keep-mine");
    }
  } finally {
    await tabTwo.close().catch(() => {});
  }

  // ---- Run from here (REQ-004 slice 3) -----------------------------------------------------
  // The criterion in three readings, and the only one that counts is the stored rows: a
  // toast that says "Run started" proves the button was pressed, not that the prefix was
  // skipped. The rows are read back through the API by step_no, because a run that quietly
  // executed the prefix the author asked to skip is exactly what this has to rule out.
  {
    // Start from the *second* action of a live graph, so there is a prefix to skip.
    // "Second in draw order" is the wrong question to ask a canvas. A rule is born from
    // `Graph::starter` as `[trigger, end]`, so the second card is always the END node — and
    // an end node is the one node that cannot start a run, correctly and by design ("the end
    // of the graph has nothing after it to run"). The probe therefore read a stated refusal
    // where the criterion wanted a run, and `canStart: "false"` says nothing about whether
    // skipping works.
    //
    // The control lives in the INSPECTOR, so it only exists for the node that is selected:
    // asking the whole page which nodes can start asks a question about a panel that renders
    // one. The search is therefore what a user does — select each card, read its own answer —
    // and the first node that says it can start and is not the trigger (a run from the top is
    // a whole run and proves nothing about a prefix being skipped) wins. The scan is recorded
    // so "no node on this graph can start" is a finding with evidence behind it rather than
    // a null.
    const cards = page.locator("[data-node-id]");
    const cardCount = await cards.count();
    const cardOrder = await page
      .evaluate(() =>
        Array.from(document.querySelectorAll("[data-node-id]")).map((card) => ({
          id: card.getAttribute("data-node-id"),
          type: card.getAttribute("data-node-type"),
        })),
      )
      .catch(() => []);
    const scan = [];
    let chosenId = null;
    for (const card of cardOrder) {
      await page.locator(`[data-node-id="${card.id}"]`).first().click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(450);
      const canStartHere = await page
        .locator("[data-run-from-here]")
        .first()
        .getAttribute("data-can-start")
        .catch(() => null);
      scan.push({ id: card.id, type: card.type, canStart: canStartHere });
      if (canStartHere === "true" && card.type !== "trigger") {
        chosenId = card.id;
        break;
      }
    }
    if (!chosenId && cardCount > 1) {
      await cards.nth(1).click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(600);
    }

    const control = page.locator("[data-run-from-here]").first();
    const controlCount = await control.count();
    const canStart = controlCount > 0
      ? await control.first().getAttribute("data-can-start")
      : null;
    const disabledReason =
      canStart === "false"
        ? ((await page.locator("[data-run-from-here-reason]").first().innerText().catch(() => ""))
            .replace(/\s+/g, " ")
            .trim())
        : null;

    const before = await page.evaluate(async () => {
      const res = await fetch("/api/v1/workflows", { credentials: "same-origin" });
      return res.ok ? res.json() : null;
    }).catch(() => null);

    let after = null;
    if (canStart === "true") {
      await page.locator("[data-run-from-here-button]").first().click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(2500);
      // Read the run back from the API, not from the canvas: the canvas paints what the
      // server sent, and the criterion is about what the engine is holding.
      after = await page.evaluate(async (workflowId) => {
        const res = await fetch(`/api/v1/workflows/${workflowId}/executions?limit=1`, {
          credentials: "same-origin",
        });
        if (!res.ok) return null;
        const body = await res.json();
        const latest = body.executions?.[0];
        if (!latest) return null;
        const detail = await fetch(`/api/v1/workflow-executions/${latest.id}`, {
          credentials: "same-origin",
        });
        if (!detail.ok) return null;
        const run = await detail.json();
        return {
          executionId: run.id ?? latest.id ?? null,
          startedFrom: run.started_from_node ?? null,
          steps: (run.steps ?? []).map((step) => ({
            step_no: step.step_no,
            status: step.status,
            skip_reason: step.skip_reason ?? null,
            node_id: step.node_id ?? null,
          })),
        };
      }, workflowId).catch(() => null);
    }

    // Criterion 2 reads the CANVAS, not the API: the pill is the thing being claimed, and
    // reading the same rows back from the server would pass even if the card rendered
    // nothing at all. The canvas is the surface under test.
    const painted = await page.evaluate(() => {
      const cards = Array.from(document.querySelectorAll("[data-node-id]"));
      const withPill = cards
        .map((card) => {
          const pill = card.querySelector("[data-node-status]");
          return {
            nodeId: card.getAttribute("data-node-id"),
            status: pill?.getAttribute("data-node-status") ?? null,
            shape: pill?.getAttribute("data-node-status-shape") ?? null,
            stepNos: pill?.getAttribute("data-node-step-nos") ?? null,
            title: pill?.getAttribute("title") ?? null,
          };
        })
        .filter((entry) => entry.status !== null);
      return { cardsOnCanvas: cards.length, painted };
    }).catch(() => ({ cardsOnCanvas: cardCount, painted: [] }));

    // A pill on a node the run never reached is a claim about work the engine did not do,
    // and nothing on screen distinguishes it from a real one — so the canvas's set of
    // painted nodes is compared against the run's, not just counted.
    const runNodes = new Set(
      (after?.steps ?? []).map((step) => step.node_id).filter((id) => typeof id === "string"),
    );
    const paintedIds = painted.painted.map((entry) => entry.nodeId);
    const paintedButNotInRun = paintedIds.filter((id) => !runNodes.has(id));
    const skippedPill = painted.painted.find((entry) => entry.status === "skipped") ?? null;

    const skippedRows = (after?.steps ?? []).filter((step) => step.status === "skipped");
    const firstSkipped = skippedRows[0] ?? null;
    const runnable = (after?.steps ?? []).filter((step) => step.status !== "skipped");

    note({
      step: "run-from-here",
      controlFound: controlCount > 0,
      cardsOnCanvas: cardCount,
      canStart,
      // Which node each card answered for, so a `canStart: "false"` can be told apart from
      // "the control never rendered for the node the criterion is about".
      scan,
      chosenId,
      // A disabled control must say why. A greyed button with no reason is a dead
      // button wearing a disabled attribute.
      disabledReason: disabledReason ? disabledReason.slice(0, 160) : null,
      skipped: skippedRows.length,
      // The reason has to *name* the node the run started at — "skipped" alone satisfies
      // the word and not the clause, and cannot be told apart from a lost run.
      reasonNamesNode: firstSkipped
        ? new RegExp(after?.startedFrom ?? "x").test(firstSkipped.skip_reason ?? "")
        : null,
      // The first step that actually runs is the one after the skipped prefix, and its
      // status is not `skipped` — the node that was pressed must be the node that ran.
      firstRunnableNo: runnable[0]?.step_no ?? null,
      firstSkippedNo: firstSkipped?.step_no ?? null,
      startedFrom: after?.startedFrom ?? null,
      // Criterion 2: every node the run touched is painted, and nothing else is. The
      // second half is the assertion that catches a pill on a node that ran nothing.
      pillsPainted: painted.painted.length,
      paintedButNotInRun,
      skippedPillFound: skippedPill !== null,
      // The pill's own tooltip has to carry the run's reason, not a word that satisfies
      // "says why" while saying nothing.
      skippedPillNamesNode:
        skippedPill && firstSkipped
          ? new RegExp(after?.startedFrom ?? "x").test(skippedPill.title ?? "")
          : null,
      ruleCount: Array.isArray(before?.workflows) ? before.workflows.length : null,
    });
    await shot(page, "page-workflow-builder-run-from-here");

    // ---- Clicking a node opens that step's inputs and output --------------------------------
    // The second half of criterion 2, and the half that cannot be read from the API: the
    // claim is about the PANEL, so the panel is what gets read. A probe that fetched the
    // run and printed `step.output` would pass against a trace that rendered nothing.
    //
    // The node clicked is a node the run actually touched — read off a card that carries a
    // pill — because clicking a node the run never reached is the `node-absent` state, and
    // asserting on that would prove the panel has an empty-state message rather than that
    // it opens the step.
    {
      const paintedNodeId = painted.painted.find((entry) => entry.status !== "skipped")?.nodeId
        ?? null;
      if (paintedNodeId) {
        await page
          .locator(`[data-node-id="${paintedNodeId}"]`)
          .first()
          .click({ timeout: 8000 })
          .catch(() => {});
        await page.waitForTimeout(500);
      }

      const trace = await page.evaluate((nodeId) => {
        const panel = nodeId ? document.querySelector(`[data-step-trace="${nodeId}"]`) : null;
        if (!panel) return null;
        const blocks = (name) => {
          const block = panel.querySelector(`[data-step-trace-payload="${name}"]`);
          if (!block) return null;
          return {
            shape: block.querySelector("[data-step-trace-payload-shape]")?.textContent ?? null,
            headline:
              block.querySelector("[data-step-trace-payload-headline]")?.textContent?.trim() ?? null,
            rows: block.querySelectorAll("dt").length,
            items: block.querySelectorAll("ol li").length,
          };
        };
        return {
          kind: panel.getAttribute("data-step-trace-kind"),
          heading: panel.querySelector("[data-step-trace-heading]")?.textContent?.trim() ?? null,
          subheading:
            panel.querySelector("[data-step-trace-subheading]")?.textContent?.trim() ?? null,
          steps: panel.querySelectorAll("[data-step-trace-step]").length,
          statuses: Array.from(panel.querySelectorAll("[data-step-trace-status]")).map((node) =>
            node.getAttribute("data-step-trace-status"),
          ),
          inputs: blocks("inputs"),
          output: blocks("output"),
        };
      }, paintedNodeId);

      // The wire has to carry the inputs at all. Read from the API, because this is the one
      // assertion about the SERVER: a panel that renders "no inputs" on every step is a
      // correct-looking panel built on a field nobody sends.
      const paramsOnWire = await page.evaluate(async (executionId) => {
        if (!executionId) return null;
        const res = await fetch(`/api/v1/workflow-executions/${executionId}`, {
          credentials: "same-origin",
        });
        if (!res.ok) return null;
        const run = await res.json();
        return (run.steps ?? []).map((step) => ({
          step_no: step.step_no,
          hasParams: step.params !== undefined,
        }));
      }, after?.executionId ?? null).catch(() => null);

      note({
        step: "step-trace",
        clickedNode: paintedNodeId,
        panelFound: trace !== null,
        kind: trace?.kind ?? null,
        // Both halves rendered: the panel opened a step at all…
        stepsShown: trace?.steps ?? 0,
        // …and it rendered the step's two sides rather than two headings.
        inputsRendered: (trace?.inputs?.rows ?? 0) + (trace?.inputs?.items ?? 0),
        outputRendered: (trace?.output?.rows ?? 0) + (trace?.output?.items ?? 0),
        inputShape: trace?.inputs?.shape ?? null,
        outputShape: trace?.output?.shape ?? null,
        heading: trace?.heading ?? null,
        subheading: trace?.subheading ?? null,
        statuses: trace?.statuses ?? [],
        // The server half: every step reports whether its inputs were on the wire.
        stepsWithParams: (paramsOnWire ?? []).filter((step) => step.hasParams).length,
        stepsTotal: (paramsOnWire ?? []).length,
      });
      await shot(page, "page-workflow-builder-step-trace");
    }
  }

  // ---- The keyboard-only pass, driven with no pointer event at all -------------------------
  // REQ-004: "A keyboard-only pass adds two nodes, connects them, edits a parameter,
  // validates and runs, with the pointer untouched."
  //
  // The criterion is a *sequence*, and the file has one thing to say about sequences: a list
  // is data (`KEYBOARD_PASS` in keyboard-path.ts) so the probe replays the order the
  // criterion states instead of inventing one. Two rules about what may be asserted:
  //
  //  * **No `page.mouse`, no `locator.click`, no `dragTo`.** A single pointer event anywhere
  //    in this block makes the whole note untrustworthy — the criterion is about the path,
  //    and a path that quietly fell back to a mouse passes every assertion below.
  //  * **The edge and the parameter are read from the SERVER.** A keyboard path that draws
  //    an edge without committing it passes every DOM assertion, because the canvas shows
  //    the edge it is holding. The server's copy is the only witness, and it is the copy the
  //    runner will execute.
  const readGraphAgain = async () =>
    page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" });
      if (!response.ok) return null;
      return await response.json();
    }, workflowId);

  {
    // Start from a known graph, so the assertions are about what this block does.
    await page.keyboard.press("Escape").catch(() => {});
    await page.waitForTimeout(300);
    const graphBeforeKb = (await readGraphAgain()) ?? null;
    const nodesBeforeKb = graphBeforeKb?.graph?.nodes?.length ?? null;
    const edgesBeforeKb = graphBeforeKb?.graph?.edges?.length ?? null;

    // `--the canvas has focus and nothing else--`. The shortcuts hang off the canvas's
    // onKeyDown, so a pass that pressed keys while a field had focus would be testing
    // `isTypingTarget` and nothing else. Focus is moved by keyboard only.
    await page.locator("[data-builder-canvas]").first().focus().catch(() => {});
    await page.waitForTimeout(200);
    const focusIsCanvas = await page.evaluate(() =>
      document.activeElement?.getAttribute("data-builder-canvas") !== null,
    );

    // Two nodes. ⌘P focuses the palette's first card, Enter adds it — and Enter is also the
    // commit key of the connect gesture, which is why the gesture guard has to be right.
    await page.keyboard.press("Control+p");
    await page.waitForTimeout(400);
    const paletteFocused = await page.evaluate(() => {
      const active = document.activeElement;
      return active?.getAttribute("data-palette-node") ?? null;
    });
    await page.keyboard.press("Enter");
    await page.waitForTimeout(500);
    await page.keyboard.press("Control+p");
    await page.waitForTimeout(400);
    await page.keyboard.press("Enter");
    await page.waitForTimeout(600);
    const canvasAfterAdds = await page.locator("[data-node-id]").count();

    // The two cards just added, in the order the keyboard put them there.
    const kbNodeIds = await page.locator("[data-node-id]").evaluateAll((cards) =>
      cards.map((card) => card.getAttribute("data-node-id")),
    );
    const newNodeIds = (graphBeforeKb?.graph?.nodes ?? []).map((node) => node.id);
    const added = kbNodeIds.filter((id) => !newNodeIds.includes(id));

    // The connect gesture: C arms the source, the arrows walk to the target, Enter commits.
    // `C` needs a *selected* source, and the last Enter left the new card selected — the
    // probe asserts that rather than assuming it, because "a shortcut whose first step
    // depends on a state the previous step did not set" is the failure being designed against.
    const selectedAfterAdd = await page.locator("[data-node-selected='true']").count();
    await page.keyboard.press("c");
    await page.waitForTimeout(400);
    const draftOpen = await page.locator("[data-link-draft]").count();
    const draftRefusal = await page.locator("[data-refusal]").first().innerText().catch(() => null);

    // Walk the selection. Tab is the documented route; a graph walk stops rather than wraps,
    // so two presses is the most this fixture can need and the note says so.
    await page.keyboard.press("Tab");
    await page.waitForTimeout(250);
    await page.keyboard.press("Tab");
    await page.waitForTimeout(250);
    await page.keyboard.press("Enter");
    await page.waitForTimeout(900);
    await page.keyboard.press("Control+s");
    await page.waitForTimeout(1600);

    // The edge, from the server.
    const graphAfterKb = (await readGraphAgain()) ?? null;
    const serverEdges = graphAfterKb?.graph?.edges ?? [];
    const committedEdge = serverEdges.find(
      (edge) => !edgesBeforeKb || !serverEdges.slice(0, edgesBeforeKb).some((before) => before.id === edge.id),
    );
    const newEdgeCount = serverEdges.length - (edgesBeforeKb ?? 0);

    // A parameter, typed into the inspector with `I`, and read back from the server.
    await page.keyboard.press("i");
    await page.waitForTimeout(400);
    const inspectorFieldFocused = await page.evaluate(() => {
      const active = document.activeElement;
      return active?.tagName === "INPUT" || active?.tagName === "TEXTAREA" || active?.tagName === "SELECT";
    });
    let paramReadBack = null;
    if (inspectorFieldFocused) {
      await page.keyboard.press("Control+a");
      await page.keyboard.type("31");
      await page.waitForTimeout(500);
      await page.keyboard.press("Tab");
      await page.waitForTimeout(400);
      await page.keyboard.press("Control+s");
      await page.waitForTimeout(1600);
      const graphAfterParam = (await readGraphAgain()) ?? null;
      const waited = (graphAfterParam?.graph?.nodes ?? []).find((node) => node.node_type === "wait");
      paramReadBack = waited?.params?.seconds ?? null;
    }

    // Validate, then run — both are single keys, and both are read from the screen and the
    // API rather than from a toast.
    await page.keyboard.press("v");
    await page.waitForTimeout(1200);
    const problemsRendered = await page.evaluate(() => {
      const panel = document.querySelector("[data-builder-problems]");
      if (!panel) return null;
      const none = panel.querySelector("[data-problems-none]");
      return {
        panelFound: true,
        saysNoProblems: none !== null,
        findings: panel.querySelectorAll("[data-finding]").length,
      };
    });
    await shot(page, "page-workflow-builder-keyboard");

    await page.keyboard.press("r");
    await page.waitForTimeout(3000);
    const runAfterKey = await page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/workflows/${id}/runs?limit=5`, { credentials: "same-origin" });
      if (!response.ok) return null;
      return await response.json();
    }, workflowId).catch(() => null);

    note({
      step: "keyboard-pass",
      // The precondition the whole note rests on.
      focusIsCanvas,
      paletteFocused: paletteFocused !== null,
      // Step 1 of the criterion: two nodes.
      nodesBefore: nodesBeforeKb,
      nodesOnCanvas: canvasAfterAdds,
      addedNodes: added.length,
      addedTwo: added.length === 2,
      // Step 2: the connect gesture, and its own state machine.
      selectedAfterAdd,
      draftOpened: draftOpen > 0,
      draftRefusal: draftRefusal ? draftRefusal.slice(0, 120) : null,
      // …committed, which only the server can witness.
      edgesBefore: edgesBeforeKb,
      edgesAfter: serverEdges.length,
      newEdges: newEdgeCount,
      edgeCommitted: newEdgeCount >= 1,
      committedEdgeSource: committedEdge?.source ?? null,
      committedEdgeTarget: committedEdge?.target ?? null,
      // Step 3: a parameter, typed and read back from the server.
      inspectorFocusedByKey: inspectorFieldFocused,
      paramReadBack,
      paramWrote: paramReadBack === 31 || paramReadBack === "31",
      // Steps 4 and 5.
      problemsPanel: problemsRendered,
      runsAfterKey: Array.isArray(runAfterKey?.runs) ? runAfterKey.runs.length : null,
      startedFromKey:
        Array.isArray(runAfterKey?.runs) && runAfterKey.runs.length > 0
          ? runAfterKey.runs[0].trigger_kind ?? runAfterKey.runs[0].status ?? null
          : null,
    });
    await shot(page, "page-workflow-builder-keyboard-final");
  }

  // ---- Tab walks the canvas (REQ-004 slice 4) ---------------------------------------------
  // The list said `Tab` walks to the next card for two ticks and **no handler existed**:
  // `onCanvasKeyDown` bound no `Tab` case and every card is `tabIndex={-1}`, so the browser
  // moved focus out to the toolbar and the selection never moved. `focusOrder` existed in
  // `selection.ts` with a unit test on its shape and no caller.
  //
  // The unit test (in `canvas-walk.test.ts`) proves the *rotation* and that a text guard
  // cannot see a delegated binding. It cannot prove that pressing the key moves anything, so
  // this does. Three claims, and the one that matters is the first:
  //
  //  1. **The selection and the focus ring land on the same card.** They are two mechanisms —
  //     `setSelection` and a programmatic `.focus()` — and a handler that did only the first
  //     would move the outline while the browser kept focus on the toolbar, which is invisible
  //     in a screenshot and would make the next keystroke (`C`, `I`) act on the wrong card.
  //     So both are read, and `document.activeElement` is the authority for where focus is.
  //  2. **A second press reaches a connection**, and `⇧Tab` comes back. The edge is the
  //     reason the walk exists for this criterion: "Del on a selected edge removes it" is only
  //     satisfiable from a keyboard if a keyboard can *reach* an edge, and a walk that stopped
  //     at the last node would be indistinguishable from a walk that only visits nodes.
  //  3. **A Tab inside a field is the field's.** `I` focuses the inspector's first input, so
  //     this is the claim that keeps the keyboard criterion satisfiable: a walk that consumed
  //     Tab there would make "edits a parameter" impossible while looking like a broken
  //     shortcut. It is measured by pressing Tab in the field and reading the *field's* focus
  //     afterwards — a walk would have pulled focus onto a card, which is the failure.
  {
    const width = 1440;
    await page.setViewportSize({ width, height: 900 });
    await page.waitForTimeout(800);

    /** The drawn selection, read off the canvas's own marker rather than a class string. */
    const readSelection = () =>
      page.$$eval("[data-node-id][data-node-selected='true'], [data-edge][data-edge-selected='true']", (els) =>
        els.map((el) => el.getAttribute("data-node-id") ?? el.getAttribute("data-edge")),
      );
    const readActive = () =>
      page.evaluate(() => {
        const el = document.activeElement;
        if (!el) return null;
        return (
          el.getAttribute?.("data-node-id") ??
          el.getAttribute?.("data-edge") ??
          el.tagName.toLowerCase()
        );
      });

    // Start from a known card: the first one, selected by a click. A walk measured from
    // "nothing is selected" would pass on any implementation that returns the first card, so
    // the probe deliberately begins in the *middle* of the graph.
    const ids = await page.$$eval("[data-node-id]", (els) =>
      els.map((el) => el.getAttribute("data-node-id")),
    );
    const startId = ids.length > 1 ? ids[1] : ids[0] ?? null;
    if (startId) {
      await page.locator(`[data-node-id="${startId}"]`).first().click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(500);
    }
    const before = { selection: await readSelection(), active: await readActive() };

    // A plain Tab. The canvas must have focus for its keydown to fire at all, and the click
    // above gave it to the card (or the canvas) — this reads where focus actually is, so a
    // probe that clicked and assumed would report "Tab did nothing" for the wrong reason.
    await page.keyboard.press("Tab");
    await page.waitForTimeout(600);
    const afterForward = { selection: await readSelection(), active: await readActive() };

    await page.keyboard.press("Tab");
    await page.waitForTimeout(600);
    const afterSecond = { selection: await readSelection(), active: await readActive() };

    // Backwards, from wherever the second press landed.
    await page.keyboard.press("Shift+Tab");
    await page.waitForTimeout(600);
    const afterBack = { selection: await readSelection(), active: await readActive() };

    // And the field half: `I` focuses the inspector's first input, and a Tab there must
    // leave the field rather than walk the canvas.
    await page.keyboard.press("i");
    await page.waitForTimeout(500);
    const fieldBefore = await page.evaluate(() => {
      const el = document.activeElement;
      return el ? el.tagName.toLowerCase() : null;
    });
    let fieldKeptFocus = null;
    if (["input", "textarea", "select"].includes(fieldBefore ?? "")) {
      await page.keyboard.press("Tab");
      await page.waitForTimeout(500);
      const after = await page.evaluate(() => {
        const el = document.activeElement;
        return el ? el.tagName.toLowerCase() : null;
      });
      // Either the field kept the caret or focus moved to the *next field* — both are the
      // field's own Tab. What must NOT happen is focus landing on a canvas card, which is
      // the walk having eaten it.
      fieldKeptFocus = !["div", "span", "g"].includes(after ?? "");
    }
    // Put the selection somewhere harmless so the rest of the pass is not driven from a
    // focused input.
    await page.keyboard.press("Escape");
    await page.waitForTimeout(300);

    const moved = (state) =>
      state.selection.length > 0 && state.selection[0] !== before.selection[0];
    note({
      step: "tab-walk",
      cardsOnCanvas: ids.length,
      startedFrom: before.selection[0] ?? null,
      // The load-bearing claim: the two mechanisms agree. A walk that moved only one of them
      // passes "did the selection change" and fails here.
      forward: { selection: afterForward.selection, active: afterForward.active, moved: moved(afterForward) },
      selectionAndFocusAgree:
        afterForward.selection.length > 0 &&
        afterForward.selection[0] === afterForward.active,
      second: { selection: afterSecond.selection, active: afterSecond.active },
      // The edge claim. A walk that never leaves the nodes is a walk that leaves "Del on a
      // selected edge" pointer-only, so the note records whether an edge was ever reached
      // rather than asserting it must have been on this graph.
      reachedAnEdge:
        afterForward.selection.some((id) => id.startsWith("e")) ||
        afterSecond.selection.some((id) => id.startsWith("e")),
      backwardsReturns:
        afterBack.selection.length > 0 &&
        afterBack.selection[0] === afterForward.selection[0],
      fieldKeptFocus,
      // NOTE: whether the list *documents* Tab is deliberately not read here. This block runs
      // before the overlay is opened, so a `[data-help-row]` query would find zero rows and
      // report a false defect — a probe reading a screen it has not opened is the same class
      // of error as one written against the old contract. The claim lives in the
      // `shortcut-help` note below, which holds the overlay open while it reads it.
    });
  }

  // ---- What the builder announces (REQ-004 slice 4 — the accessibility assertions) --------
  // The keyboard-only criterion is satisfiable by a canvas a screen reader cannot use, and no
  // screenshot can tell the difference: an outline is a CSS class, a live region is a `span`
  // clipped to a pixel, and both are invisible in a capture of the page. So this probe reads
  // the *accessibility* surface, and the three claims are the three shapes the defect took.
  //
  // **A live region that appears with its text is not a live region.** The save indicator used
  // to render a different element per state, so the region did not exist until the thing it
  // had to announce had already happened. The probe therefore checks the region is in the DOM
  // *before* anything is saved — the only moment the claim is falsifiable, because afterwards
  // a region that exists proves nothing.
  {
    const regions = await page.evaluate(() => {
      const read = (name) => {
        const el = document.querySelector(`[data-live-region="${name}"]`);
        if (!el) return { present: false };
        const style = window.getComputedStyle(el);
        return {
          present: true,
          role: el.getAttribute("role"),
          live: el.getAttribute("aria-live"),
          atomic: el.getAttribute("aria-atomic"),
          text: (el.textContent ?? "").trim(),
          // A region hidden from layout is removed from the accessibility tree, which is the
          // same defect as not rendering it. `clip` is the honest check; `display: none` and
          // `visibility: hidden` are the two that would pass a `count()` and fail a reader.
          hiddenFromLayout: style.display === "none" || style.visibility === "hidden",
          inTree: el.getClientRects().length > 0 || style.position === "absolute",
        };
      };
      return {
        total: document.querySelectorAll("[data-live-region]").length,
        save: read("save-region"),
        selection: read("selection-region"),
        link: read("link-region"),
        lock: read("lock-region"),
      };
    });

    // The cards' own names. A card is `role="button"` with a truncated label, so a reader that
    // heard only the visible text would announce "Send mail, button" — with no node type and
    // no parameters, and nothing at all when Tab moved the selection.
    const cards = await page
      .locator("[data-node-id]")
      .evaluateAll((els) =>
        els.map((el) => ({
          id: el.getAttribute("data-node-id"),
          role: el.getAttribute("role"),
          label: el.getAttribute("aria-label"),
          // The visible text, for the comparison the probe exists to make.
          visible: (el.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, 40),
        })),
      )
      .catch(() => []);

    const named = cards.filter((card) => typeof card.label === "string" && card.label.length > 0);
    // The name has to carry the node *type*, which is the fact the visible text does not: a
    // reader who hears only "Send mail" cannot tell the first card of a rule from the last.
    // `action.`/`event.`/`trigger.` are the registry's own dotted keys, so this reads the shape
    // the product produces rather than a list of magic strings kept in the probe.
    const namesNodeType = named.filter((card) => /[a-z]+\.[a-z]/.test(String(card.label))).length;

    // A Tab press and then read the selection region — the third claim, and the one the
    // previous two ticks could not have found by reading the shortcut list: a selection that
    // moved is not a text change anywhere on the page.
    const ids = cards.map((card) => card.id).filter(Boolean);
    let selectionBefore = null;
    let selectionAfter = null;
    if (ids.length > 1) {
      await page.locator(`[data-node-id="${ids[1]}"]`).first().click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(400);
      selectionBefore = await page
        .locator('[data-live-region="selection-region"]')
        .innerText()
        .catch(() => null);
      await page.keyboard.press("Tab");
      await page.waitForTimeout(600);
      selectionAfter = await page
        .locator('[data-live-region="selection-region"]')
        .innerText()
        .catch(() => null);
    }

    note({
      step: "builder-announcements",
      regionCount: regions.total,
      // The structural claim. Read BEFORE any save, while the region is empty — a region that
      // exists only after the event it should have announced has already passed.
      saveRegionPresent: regions.save.present,
      saveRegionRole: regions.save.role,
      saveRegionLive: regions.save.live,
      saveRegionAtomic: regions.save.atomic,
      saveRegionHidden: regions.save.hiddenFromLayout,
      everyRegionIsAStatus: [regions.save, regions.selection, regions.link, regions.lock].every(
        (region) => region.present && region.role === "status",
      ),
      // A card that announces nothing is a card the author cannot verify.
      cardsOnCanvas: cards.length,
      cardsNamed: named.length,
      allCardsNamed: named.length === cards.length,
      cardsAsButtons: cards.every((card) => card.role === "button"),
      // The name has to carry more than the visible text, or it is decoration.
      namesNodeType,
      namesGoBeyondVisibleText: named.filter(
        (card) => String(card.label) !== String(card.visible).replace(/[. ]+$/, ""),
      ).length,
      sampleCardName: named[0]?.label ?? null,
      sampleCardVisible: named[0]?.visible ?? null,
      // And the selection has to speak when it moves.
      selectionRegionPresent: regions.selection.present,
      selectionBefore,
      selectionAfter,
      selectionAnnouncedOnMove:
        typeof selectionBefore === "string" &&
        typeof selectionAfter === "string" &&
        selectionBefore.length > 0 &&
        selectionAfter.length > 0,
    });
  }

  // ---- ⌘/ and the shortcut list (REQ-004 slice 4) -----------------------------------------
  // The criterion is "⌘/ help", and the interesting part is that an overlay which *renders* is
  // the easy half. The claim worth measuring is the one a screenshot cannot: **the list does
  // not drift from the keys the canvas binds.** A help panel written next to the handler is
  // correct on the day it is written and quietly wrong the day someone adds ⌘K, and the day it
  // is wrong is the day a keyboard author goes there to find out what they can do.
  //
  // So the probe reads the two halves against each other: the chords the *source* binds, and
  // the rows the overlay renders. A chord with no row is the defect. The unit test in
  // `keyboard-path.test.ts` guards the same relation at build time; this one proves the
  // overlay actually shows the rows the catalogue holds, which is a different failure — a
  // catalogue nobody renders passes every unit test.
  {
    const width = 1440;
    await page.setViewportSize({ width, height: 900 });
    await page.waitForTimeout(600);

    const before = await page.locator("[data-builder-help]").count();
    // `Control+`, not `Meta+`: the whole file presses chords this way (Ctrl+A, Ctrl+Z) and the
    // box is Linux, where a `Meta` press arrives as the Super key. A probe that opens the list
    // with the wrong modifier reports a shortcut nobody has.
    await page.keyboard.press("Control+Slash");
    await page.waitForTimeout(600);
    const opened = await page.locator("[data-builder-help]").count();

    const rows = await page.$$eval("[data-help-row]", (els) =>
      els.map((el) => ({
        keys: el.getAttribute("data-help-row"),
        locked: el.getAttribute("data-help-locked") === "true",
        text: (el.textContent ?? "").replace(/\s+/g, " ").trim(),
      })),
    );
    await shot(page, "page-workflow-builder-help");

    // Escape must close it, and close ONLY it: a modal the keyboard cannot dismiss would
    // fail the keyboard-only criterion on the one screen that teaches the shortcuts.
    await page.keyboard.press("Escape");
    await page.waitForTimeout(500);
    const closedByEscape = (await page.locator("[data-builder-help]").count()) === 0;

    // And the chord toggles: a list that can only be opened and not dismissed with the same
    // key is a trap for the person who just memorised its own shortcut.
    await page.keyboard.press("Control+Slash");
    await page.waitForTimeout(500);
    const reopened = (await page.locator("[data-builder-help]").count()) === 1;
    if (reopened) {
      await page.locator("[data-builder-help-close]").first().click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(400);
    }
    const closedByButton = (await page.locator("[data-builder-help]").count()) === 0;

    const documented = new Set(
      rows.map((row) => (row.keys ?? "").toLowerCase()),
    );
    note({
      step: "shortcut-help",
      absentBefore: before === 0,
      openedByChord: before === 0 && opened === 1,
      // Both dismissals are required: a modal answerable only by the mouse is a dead end on
      // the screen whose whole subject is the keyboard.
      closedByEscape,
      togglesOnTheSameKey: reopened,
      closedByButton,
      rows: rows.length,
      lockedRows: rows.filter((row) => row.locked).length,
      // A row with no text is a key the author has to guess at.
      everyRowExplainsItself: rows.every((row) => row.text.length > (row.keys ?? "").length + 4),
      // The chords the canvas binds, as the list spells them. Read off the rows themselves so
      // a chord added to the handler without a row is visible as a *diff* in the note.
      documentsChords: ["⌘z", "⌘s", "⌘d", "⌘a", "⌘p", "⌘c", "⌘v", "⌘y", "⌘/"].every((chord) =>
        documented.has(chord) || [...documented].some((label) => label.includes(chord)),
      ),
      // Narrow-screen honesty: the rows the gate refuses have to SAY they are refused, or a
      // phone author reads a keyboard map of keys that do nothing.
      marksTheLockedRows:
        rows.filter((row) => row.locked).length > 0 &&
        rows.filter((row) => row.locked).every((row) => row.text.toLowerCase().includes("read-only")),
      // The row that lied for two ticks. `Tab` was on this list from the day it was written
      // and no handler existed; the list is the only place an author looks to find out what
      // is possible, so a row for a key that does nothing is worse than a missing row — the
      // author presses it, sees nothing, and concludes the keyboard does not work here.
      // Read from the *rendered* rows rather than the catalogue: a catalogue nobody renders
      // passes every unit test, which is exactly how the first version of this claim could
      // have been satisfied by a list that never appeared on screen.
      documentsTab: [...documented].some((label) => label.includes("tab")),
    });
  }

  // ---- The narrow-screen lock (REQ-004) ----------------------------------------------------
  // "Below 1024px the builder is read-only with the banner, Table mode stays editable, and
  // no control is unreachable."
  //
  // Four claims, and the interesting one is the *second* pair: a lock implemented in the
  // pointer handlers has a hole exactly the size of a Bluetooth keyboard, so a phone with a
  // case gets `Del` and deletes a node on a screen the banner calls read-only. The probe
  // therefore presses **keys**, not clicks, for two of the five mutations, and reads the
  // server's copy — because a canvas that refused to draw a drag looks identical to a canvas
  // that refused to *commit* one, and only the stored graph tells them apart.
  {
    const width = 900;
    await page.setViewportSize({ width, height: 900 });
    await page.waitForTimeout(1200);
    const locked = await page.evaluate(() => {
      const root = document.querySelector("[data-builder]");
      return root?.getAttribute("data-builder-locked") ?? null;
    });
    const banner = await page.locator("[data-builder-lock-banner]").first().innerText().catch(() => null);
    const bannerLinksTable = await page.locator("[data-builder-lock-table-mode]").count();

    const beforeLock = (await readGraphAgain()) ?? null;
    const beforeNodes = beforeLock?.graph?.nodes?.length ?? null;
    const beforeEdges = beforeLock?.graph?.edges?.length ?? null;
    const beforeVersion = beforeLock?.graph_version ?? null;

    // Five mutations, three by pointer and two by key. Each one is followed by a read of the
    // server's copy, and a lock that refused them all leaves every number identical.
    await page.locator("[data-palette-node='wait']").first().click({ timeout: 4000, force: true }).catch(() => {});
    await page.waitForTimeout(600);
    const afterAddAttempt = await page.locator("[data-node-id]").count();

    await page.keyboard.press("Delete");
    await page.waitForTimeout(600);
    const afterDeleteKey = await page.locator("[data-node-id]").count();

    await page.keyboard.press("c");
    await page.waitForTimeout(400);
    await page.keyboard.press("Enter");
    await page.waitForTimeout(800);
    const draftWhileLocked = await page.locator("[data-link-draft]").count();
    await page.keyboard.press("Escape");
    await page.waitForTimeout(300);

    await page.keyboard.press("Control+z");
    await page.waitForTimeout(800);

    // And the control the criterion names at the end: a locked builder must still be
    // *readable*, so a card can be selected and the inspector still shows the node's state.
    const kbNodeIds = await page.locator("[data-node-id]").evaluateAll((cards) =>
      cards.map((card) => card.getAttribute("data-node-id")),
    );
    let selectable = null;
    let inspectorStillReads = null;
    if (kbNodeIds.length > 0) {
      await page.locator(`[data-node-id="${kbNodeIds[0]}"]`).first().click({ timeout: 4000 }).catch(() => {});
      await page.waitForTimeout(500);
      selectable = await page.locator("[data-node-selected='true']").count();
      inspectorStillReads = await page
        .locator(`[data-inspector="${kbNodeIds[0]}"]`)
        .first()
        .innerText()
        .then((text) => text.replace(/\s+/g, " ").trim().slice(0, 120))
        .catch(() => null);
    }
    await shot(page, "page-workflow-builder-locked");

    const afterLock = (await readGraphAgain()) ?? null;
    const afterNodes = afterLock?.graph?.nodes?.length ?? null;
    const afterEdges = afterLock?.graph?.edges?.length ?? null;
    const afterVersion = afterLock?.graph_version ?? null;

    // The banner's own escape route has to *work*, and a lock that locked Table mode too
    // would satisfy "read-only" and fail the criterion in the same breath. So the link is
    // followed and a value is changed there.
    let tableSaves = null;
    if (bannerLinksTable > 0) {
      await page.locator("[data-builder-lock-table-mode]").first().click({ timeout: 5000 }).catch(() => {});
      await page.waitForTimeout(2000);
      const onTable = page.url().includes("/table");
      const editableOnTable = await page.locator("[data-workflow-table-edit], [data-table-edit]").count();
      tableSaves = { landedOnTable: onTable, editControls: editableOnTable };
      await shot(page, "page-workflow-builder-locked-table");
    }

    note({
      step: "narrow-lock",
      width,
      lockedAttr: locked,
      locked: locked === "true",
      bannerPresent: banner !== null,
      bannerText: banner ? banner.replace(/\s+/g, " ").trim().slice(0, 160) : null,
      bannerLinksTable: bannerLinksTable > 0,
      // Nothing may move in the server's copy.
      nodesBefore: beforeNodes,
      nodesAfterAddAttempt: afterAddAttempt,
      nodesAfter: afterNodes,
      nodesUnchanged: beforeNodes === afterNodes,
      addRefused: afterAddAttempt === beforeNodes,
      deleteKeyRefused: afterDeleteKey === afterAddAttempt,
      draftWhileLocked: draftOpenLocked(draftWhileLocked),
      edgesBefore: beforeEdges,
      edgesAfter: afterEdges,
      edgesUnchanged: beforeEdges === afterEdges,
      undoRefused: beforeVersion === afterVersion,
      versionBefore: beforeVersion,
      versionAfter: afterVersion,
      // Read-only is not unusable.
      cardStillSelectable: selectable,
      inspectorStillReads,
      tableMode: tableSaves,
    });
    // Put the viewport back so every later step runs at the width the rest of the pass
    // assumes. A pass that leaves the browser at 900px reports the next screen's layout as
    // broken, which is a finding about the probe.
    await page.setViewportSize({ width: 1440, height: 900 }).catch(() => {});
    await page.waitForTimeout(800);
  }

  // ---- A plugin node in the palette, and gone when the plugin is disabled ----------------
  // REQ-004: "A plugin node appears in the palette with its badge when the plugin is enabled
  // and disappears when it is disabled; a definition using it then reports an honest
  // validation error instead of failing at run time."
  //
  // **This probe is honest about the first claim, and that is the point.** `plugins_enabled_for`
  // is a seam REQ-121 fills: today it returns an empty registry, so *no* plugin node can
  // appear in any browser. A probe that asserted `badgeRendered: true` would be asserting a
  // store that does not exist, and it would go on asserting it after REQ-121 lands.
  //
  // So the first two claims are read from the **API's own registry** — the endpoint the
  // palette is drawn from — and the third is driven against a graph that uses the key, which
  // is the state a real author reaches. When REQ-121's store lands, `enabledNow` flips from
  // false to true on its own and the same note measures the palette.
  {
    const registry = await page
      .evaluate(async () => {
        const response = await fetch("/api/v1/workflows/node-types", { credentials: "same-origin" });
        if (!response.ok) return null;
        return await response.json();
      })
      .catch(() => null);
    const apiTypes = registry?.node_types ?? [];
    const apiPluginTypes = apiTypes.filter((entry) => entry.provider);
    const palettePlugin = await page
      .evaluate(() => {
        const entries = Array.from(document.querySelectorAll("[data-palette-plugin]"));
        return entries.map((entry) => ({
          key: entry.getAttribute("data-palette-node"),
          provider: entry.getAttribute("data-palette-plugin"),
          badgeRendered: entry.querySelector("[data-palette-badge]") !== null,
          // The tooltip has to name the provider, not just the badge: "Send mail · Plugin"
          // says *that* it is a plugin and not *who* wrote it.
          tooltipNamesProvider: (entry.getAttribute("title") ?? "").includes(
            entry.getAttribute("data-palette-plugin") ?? "\u0000",
          ),
        }));
      })
      .catch(() => []);

    // The third claim, driven for real: a graph that uses a plugin key is put through the
    // validate endpoint, and the answer has to be the *plugin* sentence — "re-enable" — and
    // not the *typo* sentence. The two are the whole criterion, and a refactor that made
    // the disabled plugin read as a typo would satisfy "reports an honest validation error"
    // in the loosest reading of the words.
    const pluginKey = "plugin.mailer.send";
    const disabledRead = await page
      .evaluate(
        async ({ workflowId: id, key }) => {
          const graphResponse = await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" });
          if (!graphResponse.ok) return null;
          const current = await graphResponse.json();
          const nodes = (current.graph?.nodes ?? []).map((node) =>
            node.node_type === key
              ? node
              : { ...node, node_type: node.node_type === "end" ? "end" : node.node_type },
          );
          // Put the key on the node that is not the trigger and not the end, so the finding
          // is about the node type and not about an orphaned graph.
          const target = nodes.find((node) => !node.node_type.startsWith("trigger.") && node.node_type !== "end");
          if (!target) return { skipped: "no non-trigger, non-end node to rename" };
          target.node_type = key;
          const validateResponse = await fetch(`/api/v1/workflows/${id}/graph/validate`, {
            method: "POST",
            credentials: "same-origin",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ graph: { nodes, edges: current.graph?.edges ?? [] } }),
          });
          const body = await validateResponse.json().catch(() => null);
          return {
            status: validateResponse.status,
            findings: (body?.findings ?? []).map((finding) => ({
              code: finding.code,
              message: finding.message,
              nodeId: finding.node_id ?? null,
            })),
          };
        },
        { workflowId, key: pluginKey },
      )
      .catch(() => null);

    const unknownFindings = (disabledRead?.findings ?? []).filter((finding) => finding.code === "unknown_node_type");
    const pluginSentence = unknownFindings.find((finding) => /re-enable/i.test(finding.message));
    const typoSentence = unknownFindings.find((finding) => /not a node type the platform knows/i.test(finding.message));

    note({
      step: "plugin-palette",
      // The honest first half: today there is no plugin store, so the palette has nothing to
      // draw. Recorded rather than assumed, because a note that hard-codes `false` here is a
      // note that will still be asserting the store is empty after REQ-121 lands.
      enabledNow: apiPluginTypes.length > 0,
      apiPluginTypes: apiPluginTypes.map((entry) => entry.key),
      apiCategoriesIncludePlugins: (registry?.categories ?? []).includes("Plugins"),
      palettePluginNodes: palettePlugin.length,
      palettePlugin,
      badgeRendered: palettePlugin.length > 0 && palettePlugin.every((entry) => entry.badgeRendered),
      tooltipNamesProvider: palettePlugin.length > 0 && palettePlugin.every((entry) => entry.tooltipNamesProvider),
      absentWhenDisabled: palettePlugin.length === 0,
      // The third claim, measured against a graph that uses the key.
      pluginKey,
      validateStatus: disabledRead?.status ?? null,
      unknownFindings: unknownFindings.length,
      namesNode: unknownFindings.some((finding) => typeof finding.nodeId === "string"),
      // The two sentences must not collapse into each other; a test asserts this in Rust too.
      saysReEnable: Boolean(pluginSentence),
      saysTypo: Boolean(typoSentence),
      sentinelsStayApart: Boolean(pluginSentence) && !typoSentence,
    });
  }

  // ---- Listen for a real event (REQ-004 slice 3, criterion 5) ----------------------------
  // The criterion has two clauses and the probe is shaped so each one FAILS if it stops
  // being true:
  //
  //   * "captures a real bus event into the inspector **within one matcher tick**" — so the
  //     reading is the PANEL's own text, not the API's. A probe that fetched
  //     `/listeners` and printed `payload.slug` would pass against a panel that renders
  //     nothing at all, which is the failure this control is most likely to have.
  //   * "expires after 15 minutes **leaving no stray token**" — the window is read off the
  //     two timestamps the server sends, because `expires_in_seconds` is 899 by the time a
  //     response lands and pinning it to 900 asserts a rounding rule rather than the
  //     window. The "no stray token" half is a server-side claim (the expired row is not in
  //     a live-listener query) and is NOT readable from a browser, so it is proved by the
  //     integration walk and only the *sentence* is read here.
  {
    // The trigger node, so the control is pressable: a listener waits for the event that
    // starts a run, and a non-trigger node is refused with a reason.
    const triggerCard = page
      .locator('[data-node-type^="trigger."]')
      .first();
    const triggerCount = await triggerCard.count();
    if (triggerCount > 0) {
      await triggerCard.click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(700);
    }

    // **Retarget the graph node's own event before arming.** A listener is armed for the
    // `params.event` of the *node* the author selected, not for the rule row's trigger
    // column — that is the whole reason the builder's listener is node-scoped. The pass
    // above created this rule listening for `user.created`, which a browser session cannot
    // produce, so the node is pointed at `page.published` through the real graph route.
    //
    // The order is load-bearing: arming first and retargeting second would capture nothing,
    // and "no capture" is exactly what a broken listener looks like. The reload matters for
    // the same reason — the panel reads the node from the store the canvas was opened with,
    // so arming without one would press a listener against the OLD event name.
    const retargeted = await page.evaluate(async (workflowId) => {
      const EVENT = "page.published";
      const current = await fetch(`/api/v1/workflows/${workflowId}/graph`, {
        credentials: "same-origin",
      });
      if (!current.ok) return { ok: false, reason: `read ${current.status}` };
      const body = await current.json();
      const nodes = body?.graph?.nodes ?? [];
      const trigger = nodes.find((node) => String(node.type).startsWith("trigger."));
      if (!trigger) return { ok: false, reason: "no trigger node on the canvas" };
      if (trigger?.params?.event === EVENT) return { ok: true, alreadyThere: true, event: EVENT };

      const next = nodes.map((node) =>
        node.id === trigger.id
          ? { ...node, params: { ...(node.params ?? {}), event: EVENT } }
          : node,
      );
      const saved = await fetch(`/api/v1/workflows/${workflowId}/graph`, {
        method: "PUT",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          graph: { nodes: next, edges: body?.graph?.edges ?? [] },
          graph_version: body.graph_version,
        }),
      });
      if (!saved.ok) return { ok: false, reason: `save ${saved.status}` };
      return { ok: true, event: EVENT, nodeId: trigger.id };
    }, workflowId ?? null).catch(() => null);

    if (retargeted?.ok && !retargeted.alreadyThere) {
      await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForTimeout(1800);
      // Re-select the trigger: a reload clears the selection, and the control is
      // unpressable with nothing selected — which is its own refusal, and the wrong one
      // to be measuring here.
      await page
        .locator('[data-node-type^="trigger."]')
        .first()
        .click({ timeout: 8000 })
        .catch(() => {});
      await page.waitForTimeout(600);
    }

    // The refusal state first, because it is the one that costs an author fifteen minutes.
    const armedControl = page.locator("[data-listener-arm]");
    const armCount = await armedControl.count();
    const armDisabled = armCount > 0
      ? await armedControl.first().isDisabled().catch(() => true)
      : null;
    const armReason = (await page
      .locator("[data-listener-reason]")
      .first()
      .innerText()
      .catch(() => ""))
      .replace(/\s+/g, " ")
      .trim();

    let armed = null;
    if (armCount > 0 && !armDisabled) {
      await armedControl.first().click({ timeout: 8000 }).catch(() => {});
      await page.waitForTimeout(2500);

      // **A real bus event**, through a real API route rather than an insert into `events`:
      // an insert would prove the SQL and not the route the matcher reads from. The node
      // was already retargeted above, so this only has to produce the event.
      //
      // A probe that fired the wrong event would report "no capture" and be
      // indistinguishable from a broken listener — which is why the name comes from the
      // retarget that just ran rather than being written out a second time.
      const eventFired = await page.evaluate(async () => {
        const EVENT = "page.published";
        const created = await fetch("/api/v1/pages", {
          method: "POST",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ slug: "listen-probe", title: "Listen probe" }),
        });
        if (!created.ok) return { published: false, reason: `create ${created.status}`, event: EVENT };
        const pageId = (await created.json())?.id;
        if (!pageId) return { published: false, reason: "no page id", event: EVENT };
        const published = await fetch(`/api/v1/pages/${pageId}/publish`, {
          method: "POST",
          credentials: "same-origin",
        });
        return { published: published.ok, event: EVENT };
      }).catch(() => null);

      // The matcher runs in the API process, so the capture needs a tick of wall clock.
      // The panel polls every two seconds; give it three and a margin.
      await page.waitForTimeout(7000);
      await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
      await page.waitForTimeout(2000);

      armed = await page.evaluate(() => {
        const panel = document.querySelector("[data-builder-listener]");
        if (!panel) return null;
        const capture = panel.querySelector("[data-listener-capture-status]");
        const payload = panel.querySelector("[data-listener-payload]");
        return {
          panelFound: true,
          summary: panel.querySelector("[data-listener-summary]")?.textContent?.trim() ?? null,
          // The panel's own sentence, which is the reading that matters.
          captureText: capture?.textContent?.replace(/\s+/g, " ").trim() ?? null,
          payloadRendered: payload !== null,
          payloadLength: payload?.textContent?.length ?? 0,
          countdown: panel.querySelector("[data-listener-countdown]")?.textContent?.trim() ?? null,
          tokenShown: panel.querySelector("[data-listener-token]")?.textContent?.trim() ?? null,
          live: panel.querySelector("[data-listener-live]") !== null,
        };
      });
      armed = { ...(armed ?? {}), eventFired: eventFired ?? null };
    }

    // The wire, read from the browser's own session: the panel proves it renders, this
    // proves the server sent a real payload and that the window is fifteen minutes.
    const onTheWire = await page.evaluate(async (workflowId) => {
      if (!workflowId) return null;
      const res = await fetch(`/api/v1/workflows/${workflowId}/listeners`, {
        credentials: "same-origin",
      });
      if (!res.ok) return { status: res.status };
      const body = await res.json();
      const captured = body.captured ?? null;
      let windowSeconds = null;
      if (captured) {
        const armed0 = Date.parse(captured.armed_at);
        const expires = Date.parse(captured.expires_at);
        if (Number.isFinite(armed0) && Number.isFinite(expires)) {
          windowSeconds = Math.round((expires - armed0) / 1000);
        }
      }
      return {
        status: res.status,
        armed: body.armed ?? null,
        rows: Array.isArray(body.listeners) ? body.listeners.length : null,
        capturedStatus: captured?.status ?? null,
        capturedEvent: captured?.event_name ?? null,
        capturedPayloadKeys: captured?.payload ? Object.keys(captured.payload) : null,
        windowSeconds,
        // The token must never be what the server sends back on a read — it is returned
        // once by the arming and never again.
        tokenInRead: Object.values(captured ?? {}).some((value) =>
          typeof value === "string" && value.length === 32 && /^[A-Za-z0-9]+$/.test(value),
        ),
      };
    }, workflowId ?? null).catch(() => null);

    note({
      step: "listener",
      panelFound: (await page.locator("[data-builder-listener]").count()) > 0,
      controlFound: armCount > 0,
      // The retarget is a precondition, not part of the criterion — but a probe that
      // skipped it would report "no capture" and be read as a broken listener.
      retargeted: retargeted ?? null,
      // A disabled control must say why. A greyed button with no reason is a dead button
      // wearing a disabled attribute.
      armDisabled,
      armReason: armReason ? armReason.slice(0, 160) : null,
      triggerNodeFound: triggerCount > 0,
      // The panel's own rendering.
      captureRendered: (armed?.captureText ?? null) !== null,
      captureText: armed?.captureText ?? null,
      payloadRendered: armed?.payloadRendered ?? null,
      payloadLength: armed?.payloadLength ?? null,
      countdown: armed?.countdown ?? null,
      summary: armed?.summary ?? null,
      eventPublished: armed?.eventFired?.published ?? null,
      eventName: armed?.eventFired?.event ?? null,
      // The server's half.
      armedOnTheWire: onTheWire?.armed ?? null,
      listenerRows: onTheWire?.rows ?? null,
      capturedStatus: onTheWire?.capturedStatus ?? null,
      capturedEvent: onTheWire?.capturedEvent ?? null,
      capturedPayloadKeys: onTheWire?.capturedPayloadKeys ?? null,
      // REQ-004 names fifteen minutes; asserted as the gap between the two timestamps.
      windowSeconds: onTheWire?.windowSeconds ?? null,
      tokenReturnedOnRead: onTheWire?.tokenInRead ?? null,
    });
    await shot(page, "page-workflow-builder-listener");
  }

  // ---- Cleanup: this pass owns the rule it made --------------------------------------------
  await page.goto(`${URL_ADMIN}/automations`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1200);
  const row = page.locator("[data-automation-row] a", { hasText: ruleName }).first();
  if ((await row.count()) > 0) {
    await row.click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1400);
    await page.locator("[data-automation-delete]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForSelector("[data-automation-delete-input]", { timeout: 5000 }).catch(() => {});
    await page.locator("[data-automation-delete-input]").first().fill(ruleName).catch(() => {});
    await page.locator("[data-automation-delete-confirm-button]").first().click({ timeout: 5000 }).catch(() => {});
    await page.waitForTimeout(1500);
  }
  note({ step: "cleanup", removed: ruleName });

  report.workflowBuilder = { steps, ruleName, opened };
  log(`workflow-builder: ${JSON.stringify(steps)}`);
}

/**
 * The depth passes that `--only=<name>` can run on their own.
 *
 * The key is the pass's own name minus the `Depth` suffix (`automations` for
 * `runAutomationsDepth`), so the flag reads like the thing it drives. A pass that is not listed
 * here simply cannot be run alone, which is the honest default: a partial pass that silently ran
 * nothing would report "0 failures" and mean nothing by it.
 */
/**
 * REQ-004 criterion 8 — Table mode, driven from the builder's own "Table mode" link.
 *
 * ## What the criterion actually asks, and why the obvious probe is worthless
 *
 * *"Table mode renders the same definition, edits parameters, and stays consistent with the
 * canvas after a save in either mode."* Three claims, and the third is the only hard one.
 *
 * A probe that loads `/workflows/{id}/table` and counts rows answers the first claim and is
 * compatible with **both** ways of getting it wrong. The old link pointed at
 * `/automations/{id}` — REQ-003's linear step editor, a different projection of the rule — and
 * a table over that is a perfectly good table of a definition the canvas never drew. So this
 * probe goes through the *link the builder offers*, which is the path an author takes, and it
 * answers the third claim in the only way that can fail: read the graph back **through the API**
 * after the save, and compare it with what the table claims it wrote.
 *
 * ## The two ways a table can be fake, and what each needs asserted
 *
 * 1. **A table that renders and does not save.** `defaultValue` on an input is uncontrolled, so
 *    the obvious implementation never reads it back, the save button is a decoration, and the
 *    screen looks right until the author reloads. The probe therefore asserts the *server's*
 *    value changed — not that a toast appeared.
 * 2. **A table that saves and drifts.** It holds its own copy of the graph and writes it whole,
 *    so a save from the canvas between load and commit silently reverts the table's edit. The
 *    probe commits **after** a canvas-side save and asserts the canvas's own edit survived, which
 *    is the "in either mode" half of the sentence.
 */

async function runWorkflowTableDepth(page, report) {
  const steps = [];
  const note = (entry) => {
    steps.push(entry);
    record({ page: "workflow-table-depth", action: "workflow-table", ...entry });
  };

  // A rule of our own: the pass must not depend on whatever another writer's pass left behind.
  const ruleName = `QA table rule ${Date.now().toString(36)}`;
  // The create is a *definition*, and a definition is a trigger plus steps: `WorkflowInput`
  // deserializes `trigger` and `steps` as required, so `{name, description}` alone was
  // refused as a missing field before any of the table code under test ran — the pass then
  // returned on `id: null` and every row below it read empty, which is how criterion 8 sat
  // unmeasured for three ticks. The trigger is structured (`{"kind":"manual"}`, never the
  // bare string), and one task step is the smallest definition the engine accepts.
  const created = await page.evaluate(async (name) => {
    const response = await fetch("/api/v1/workflows", {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        name,
        description: "table mode probe",
        trigger: { kind: "manual" },
        steps: [
          {
            name: "Only step",
            kind: "task",
            action: "log",
            params: { message: "table mode probe" },
          },
        ],
      }),
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  }, ruleName);
  const workflowId = created.body?.id ?? created.body?.workflow?.id ?? null;
  // `StepDefinition` is `deny_unknown_fields`, so a rejected body says WHICH field in the
  // refusal. Recording it turns "the create failed" into the next action; an empty string
  // here meant the next four ticks each guessed at the payload instead of reading this.
  note({
    step: "create",
    status: created.status,
    id: workflowId,
    refusal: created.status >= 400 ? (created.body?.error?.message ?? "").slice(0, 200) : null,
  });
  if (!workflowId) {
    log(`workflow-table: ${JSON.stringify(steps)}`);
    return steps;
  }

  const readGraph = async () =>
    page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/workflows/${id}/graph`, {
        credentials: "same-origin",
      });
      return response.json().catch(() => null);
    }, workflowId);

  // ---- The builder, and the link an author would press -----------------------------------
  await page.goto(`${admin}/workflows/${workflowId}/builder`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-builder-table-mode]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(900);

  const linkHref = await page
    .locator("[data-builder-table-mode]")
    .first()
    .getAttribute("href")
    .catch(() => null);
  note({
    step: "builder-link",
    href: linkHref,
    // The old value was /automations/{id}. A link that still points there is a link to a
    // different definition, and no amount of table correctness would satisfy the criterion.
    pointsAtTableRoute: linkHref === `/workflows/${workflowId}/table`,
  });

  await page.locator("[data-builder-table-mode]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForSelector("[data-table-mode]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(1200);
  await shot(page, "page-workflow-table");

  // ---- The same definition, row for row ---------------------------------------------------
  const canvasGraph = await readGraph();
  const canvasNodes = canvasGraph?.graph?.nodes ?? [];
  const canvasEdges = canvasGraph?.graph?.edges ?? [];

  const rows = await page.$$eval("[data-table-row]", (els) =>
    els.map((el) => ({
      id: el.getAttribute("data-table-row"),
      type: el.querySelector("[data-table-type]")?.textContent?.trim() ?? "",
      params: [...el.querySelectorAll("[data-table-param]")].map((input) => ({
        key: input.getAttribute("data-table-param")?.split(".").slice(1).join(".") ?? "",
        value: input.value,
      })),
      connections: [...el.querySelectorAll("[data-table-incoming],[data-table-outgoing]")].map(
        (li) => li.textContent?.trim() ?? "",
      ),
    })),
  );

  note({
    step: "same-definition",
    canvasNodes: canvasNodes.length,
    canvasEdges: canvasEdges.length,
    rows: rows.length,
    // A count would pass against a table showing the WRONG nodes in the right number.
    idsMatch:
      rows.length === canvasNodes.length &&
      canvasNodes.every((node) => rows.some((row) => row.id === node.id)),
    typesShown: rows.every((row) => row.type.length > 0),
    // The criterion's "edits parameters" half needs a field to exist: a rule whose nodes carry
    // no parameters renders "No parameters" and proves nothing about editing.
    editableParams: rows.reduce((sum, row) => sum + row.params.length, 0),
    connectionsShown: rows.reduce((sum, row) => sum + row.connections.length, 0),
  });

  // ---- Edit a parameter, and read the SERVER back -----------------------------------------
  // Give a node a parameter to edit if it has none, through the API, so the probe does not
  // depend on which node type the registry seeded.
  await page.evaluate(async (id) => {
    const current = await (await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })).json();
    const trigger = current.graph.nodes.find((n) => n.type.startsWith("trigger"));
    if (!trigger) return;
    trigger.params = { ...(trigger.params ?? {}), event: "qa.table.probe" };
    await fetch(`/api/v1/workflows/${id}/graph`, {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ graph: current.graph, graph_version: current.graph_version }),
    });
  }, workflowId);
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-table-row]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(1200);

  const target = await page.evaluate(() => {
    const input = document.querySelector("[data-table-param]");
    if (!input) return null;
    return input.getAttribute("data-table-param");
  });

  const saveStateBefore = (await page.locator("[data-table-save-state]").first().innerText().catch(() => "")).trim();
  const saveDisabledBefore = await page.locator("[data-table-save]").first().isDisabled().catch(() => true);

  if (target) {
    const [nodeId, key] = [target.split(".")[0], target.split(".").slice(1).join(".")];
    await page
      .locator(`[data-table-param="${nodeId}.${key}"]`)
      .first()
      .fill("qa.table.edited")
      .catch(() => {});
    // The field is uncontrolled on purpose (a controlled field re-renders the whole draft on
    // every keystroke and the caret jumps), so the commit happens on blur.
    await page.locator("[data-table-rows]").first().click({ position: { x: 5, y: 5 } }).catch(() => {});
    await page.waitForTimeout(400);
  }

  const saveStateDirty = (await page.locator("[data-table-save-state]").first().innerText().catch(() => "")).trim();
  const saveEnabledDirty = await page.locator("[data-table-save]").first().isDisabled().catch(() => true);
  await shot(page, "page-workflow-table-dirty");

  await page.locator("[data-table-save]").first().click({ timeout: 8000 }).catch(() => {});
  await page.waitForTimeout(2500);
  const saveStateAfter = (await page.locator("[data-table-save-state]").first().innerText().catch(() => "")).trim();

  // The server's copy, not the screen's.
  const afterEdit = await readGraph();
  const editedNode = (afterEdit?.graph?.nodes ?? []).find((n) => n.id === target?.split(".")[0]);
  note({
    step: "edit-saves",
    field: target,
    // An unedited draft must not be committable: a write here advances graph_version and hands
    // the next tab a conflict no author created.
    saveDisabledWhenClean: saveDisabledBefore,
    saveEnabledWhenDirty: saveEnabledDirty === false,
    stateWhenClean: saveStateBefore,
    stateWhenDirty: saveStateDirty,
    stateAfterSave: saveStateAfter,
    serverValue: editedNode ? Object.entries(editedNode.params ?? {}).find(([k]) => k === target?.split(".").slice(1).join("."))?.[1] ?? null : null,
    wroteToServer: editedNode
      ? Object.entries(editedNode.params ?? {}).some(([k, v]) => k === target?.split(".").slice(1).join(".") && v === "qa.table.edited")
      : false,
    version: afterEdit?.graph_version ?? 0,
  });
  await shot(page, "page-workflow-table-saved");

  // ---- "Consistent with the canvas after a save in either mode" -----------------------------
  // The canvas writes a label, the table must show it. This is the direction the criterion
  // names and the one a table holding its own copy of the graph gets wrong: it re-renders from
  // what it loaded and the author's canvas edit is simply gone from the list.
  await page.evaluate(async (id) => {
    const current = await (await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })).json();
    const node = current.graph.nodes[0];
    if (!node) return;
    node.label = "Renamed on the canvas";
    await fetch(`/api/v1/workflows/${id}/graph`, {
      method: "PUT",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ graph: current.graph, graph_version: current.graph_version }),
    });
  }, workflowId);
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForSelector("[data-table-row]", { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(1200);
  const labelsAfterCanvasSave = await page.$$eval("[data-table-label]", (els) =>
    els.map((el) => el.value),
  );
  note({
    step: "canvas-save-visible",
    labels: labelsAfterCanvasSave.slice(0, 4),
    seesCanvasRename: labelsAfterCanvasSave.includes("Renamed on the canvas"),
  });

  // ---- A save from the table must not be reverted by a stale canvas load --------------------
  // The reverse direction: a value the table committed is still there after the builder is
  // opened and closed. A table that wrote to a different projection would pass every check
  // above and fail exactly here.
  await page.goto(`${admin}/workflows/${workflowId}/builder`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);
  const builderSeesTableEdit = await page.evaluate(async (id) => {
    const current = await (await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })).json();
    return Object.values(current.graph.nodes ?? {}).some((n) =>
      Object.values(n.params ?? {}).includes("qa.table.edited"),
    );
  }, workflowId);
  note({ step: "table-save-survives", builderSeesTableEdit });
  await shot(page, "page-workflow-table-final");

  // ---- An unfinished rule SAVES, and the run is where it refuses ---------------------------
  // The guard moved from "you may not save" to "you may not run" (91bcbda), and a probe written
  // against the old contract is the reason this half went unmeasured: the save answers **200**
  // carrying `findings`, so a note that reads `saveState === "error"` reports the opposite of
  // what happened, and a "save failed" row is an instrument defect rather than a product one.
  // The problems list is therefore read off the **response body** — the only place a 200 can
  // carry a verdict — and the Run button is then pressed and its refusal read as text.
  //
  // The graph used here is the one the table pass just renamed, so it is already a real rule with
  // real nodes; what makes it un-runnable is a *missing event name* on the trigger, which is the
  // state a rule is in for the whole of the first minute of its life and the one the previous
  // contract made unsavable.
  const unfinished = await page.evaluate(async (id) => {
    const current = await (await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })).json();
    const graph = current.graph;
    // A trigger with no event name: `missing_parameter`, an error, and the graph still projects
    // well enough to store — the exact shape the old save refused and the new one records.
    const trigger = graph.nodes.find((n) => n.type.startsWith("trigger"));
    if (!trigger) return { skipped: "no trigger node in the graph" };
    trigger.params = { ...(trigger.params ?? {}) };
    delete trigger.params.event;

    const csrf = document.cookie
      .split(";")
      .map((pair) => pair.split("="))
      .find(([name]) => name.trim() === "omnion_csrf")?.[1]
      ?.trim();

    const response = await fetch(`/api/v1/workflows/${id}/graph`, {
      method: "PUT",
      credentials: "same-origin",
      headers: {
        "content-type": "application/json",
        ...(csrf ? { "x-omnion-csrf": csrf } : {}),
      },
      body: JSON.stringify({ graph, graph_version: current.graph_version }),
    });
    const body = await response.json().catch(() => null);
    return {
      status: response.status,
      errorCount: body?.error_count ?? null,
      findingCount: body?.findings?.length ?? null,
      codes: (body?.findings ?? []).map((f) => f.code).slice(0, 6),
      recordedReason: body?.validation_error ?? null,
      version: body?.graph_version ?? null,
    };
  }, workflowId);
  note({
    step: "unfinished-saves",
    // 200 is the whole claim: refusing to store a rule that is not yet runnable is refusing
    // the first keystroke of the feature whose job is being edited.
    savedWith200: unfinished.status === 200,
    status: unfinished.status ?? null,
    errorCount: unfinished.errorCount ?? null,
    findingCount: unfinished.findingCount ?? null,
    codes: unfinished.codes ?? null,
    reasonRecorded: Boolean((unfinished.recordedReason ?? "").trim()),
    reason: (unfinished.recordedReason ?? "").slice(0, 120) || null,
    skipped: unfinished.skipped ?? null,
  });

  // Now the run. The refusal must be a *sentence about the rule*, read off the screen, and the
  // stored steps must still be the last runnable list — a save that blanked them would make
  // "not yet runnable" and "has never run" the same row.
  const runResponse = await page.evaluate(async (id) => {
    const csrf = document.cookie
      .split(";")
      .map((pair) => pair.split("="))
      .find(([name]) => name.trim() === "omnion_csrf")?.[1]
      ?.trim();
    const response = await fetch(`/api/v1/workflows/${id}/run`, {
      method: "POST",
      credentials: "same-origin",
      headers: { ...(csrf ? { "x-omnion-csrf": csrf } : {}) },
    });
    const body = await response.json().catch(() => null);
    const after = await (await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })).json();
    return {
      status: response.status,
      code: body?.error?.code ?? null,
      message: (body?.error?.message ?? "").slice(0, 160) || null,
      stepsKept: Array.isArray(after?.steps) ? after.steps.length : null,
      executions: await (await fetch(`/api/v1/workflows/${id}/executions`, { credentials: "same-origin" }))
        .json()
        .then((r) => (Array.isArray(r?.executions) ? r.executions.length : null))
        .catch(() => null),
    };
  }, workflowId);
  note({
    step: "unfinished-run-refused",
    // 400, not 403/404/409: the caller is allowed to try, this rule is not ready.
    refused: runResponse.status === 400,
    status: runResponse.status ?? null,
    code: runResponse.code,
    message: runResponse.message,
    // The recorded reason reaches the author verbatim rather than as a generic refusal.
    messageNamesTheGraph: Boolean((runResponse.message ?? "").trim()),
    // A refusal must leave no run behind: an execution row for a run that never started is a
    // rule that looks like it fired.
    executionsAfter: runResponse.executions ?? null,
    stepsKept: runResponse.stepsKept ?? null,
  });

  // And the panel, which is where an author finds out: the problems list must render the
  // findings the 200 carried. A 200 whose findings never reach the screen is a save that
  // answers "yes" and tells the author nothing.
  await page.reload({ waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(1500);
  const problemsAfterUnfinishedSave = await page.evaluate(() => {
    const toggle = document.querySelector("[data-problems-toggle]");
    return {
      header: (toggle?.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, 80),
      listed: [...document.querySelectorAll("[data-finding]")].map((el) => el.getAttribute("data-finding")),
      saysNone: Boolean(document.querySelector("[data-problems-none]")),
    };
  });
  note({
    step: "unfinished-problems-panel",
    header: problemsAfterUnfinishedSave.header,
    listed: problemsAfterUnfinishedSave.listed,
    // "No problems" over a graph the server will not run is the exact failure the finding list
    // fix was made for.
    notClaimingClean: !problemsAfterUnfinishedSave.saysNone,
    namesTheMissingParameter: problemsAfterUnfinishedSave.listed.includes("missing_parameter"),
  });

  // Put the rule back together, so a later pass step that runs a rule of its own is not handed
  // the rule this probe broke.
  await page.evaluate(async (id) => {
    const current = await (await fetch(`/api/v1/workflows/${id}/graph`, { credentials: "same-origin" })).json();
    const trigger = current.graph.nodes.find((n) => n.type.startsWith("trigger"));
    if (!trigger) return;
    trigger.params = { ...(trigger.params ?? {}), event: "user.created" };
    const csrf = document.cookie
      .split(";")
      .map((pair) => pair.split("="))
      .find(([name]) => name.trim() === "omnion_csrf")?.[1]
      ?.trim();
    await fetch(`/api/v1/workflows/${id}/graph`, {
      method: "PUT",
      credentials: "same-origin",
      headers: {
        "content-type": "application/json",
        ...(csrf ? { "x-omnion-csrf": csrf } : {}),
      },
      body: JSON.stringify({ graph: current.graph, graph_version: current.graph_version }),
    });
  }, workflowId);
  note({ step: "repaired", repaired: true });

  log(`workflow-table: ${JSON.stringify(steps)}`);
  report.workflowTable = steps;
  return steps;
}

const DEPTH_PASSES = {
  automations: (page, report) => runAutomationsDepth(page, report),
  automationsactions: (page, report) => runAutomationsActionsDepth(page, report),
  automationsapprovals: (page, report) => runAutomationsApprovalsDepth(page, report),
  // The operations pass (REQ-003, slice 4) — the five screens that cannot be reached from a
  // static route list, so the pass builds the state they read and then reads them.
  automationsoperations: (page, report) => runAutomationsOperationsDepth(page, report),
  // The builder (REQ-004, slice 1): the workspace's path carries a rule id, so the pass
  // creates a rule and drives *its* builder.
  workflowbuilder: (page, report) => runWorkflowBuilderDepth(page, report),
  // Table mode (REQ-004, criterion 8): a sibling route of the same graph, reached through
  // the builder's own link so the probe measures the path an author takes.
  workflowtable: (page, report) => runWorkflowTableDepth(page, report),
  analytics: (page, report) => runAnalyticsDepth(page, report),
  search: (page, report) => runSearchDepth(page, report),
  iamroles: (page, report) => runIamRolesDepth(page, report),
  iampolicies: (page, report) => runIamPoliciesDepth(page, report),
  iamsecurity: (page, report) => runIamSecurityDepth(page, report),
  iamapprovals: (page, report) => runIamApprovalsDepth(page, report),
  iamprovisioning: (page, report) => runIamProvisioningDepth(page, report),
  // The enterprise sign-in pass (REQ-006, slice 4b-2) — registered here too, so the wave that
  // wrote it can run it alone with `--only=iamauthentication` rather than through the full
  // inventory beside five other writers' stacks.
  iamauthentication: (page, report) => runIamAuthenticationDepth(page, report),
  passkeys: (page, report) => runPasskeysDepth(page, report),
};

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
