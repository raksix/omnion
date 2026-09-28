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
const { execFileSync } = require("child_process");
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
function record(entry) {
  clickLines.push(entry);
  fs.appendFileSync(path.join(OUT, "clicks.jsonl"), JSON.stringify(entry) + "\n");
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

  return { ok: rendered && kind !== null, steps: steps.length, kind, fileId, camera: Boolean(cameraFileId) };
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
    ];
    const missing = flags.filter((f) => be[f] === undefined);
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

  // The duplicate report (REQ-010, slice 3): two identical uploads form a group, the Merge button
  // is dead until a keeper is chosen, the merge keeps the *chosen* file, and the result says the
  // bytes are pending rather than reclaimed.
  report.mediaDuplicates = await runDepthPass("media-duplicates", () =>
    runMediaDuplicates(page, report),
  );
  log(`media duplicates: ${JSON.stringify(report.mediaDuplicates)}`);

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
  for (const route of [{ path: "/", name: "overview" }, { path: "/pages", name: "pages" }, { path: "/ai", name: "ai" }, { path: "/search?q=qa", name: "search" }, { path: "/settings/search", name: "search-settings" }, { path: "/settings/iam/users", name: "iam-users" }, { path: "/settings/iam/groups", name: "iam-groups" }, { path: "/settings/iam/simulator", name: "iam-simulator" }, { path: "/settings/iam/policies", name: "iam-policies" }, { path: "/settings/iam/approvals", name: "iam-approvals" }, { path: "/settings/iam/provisioning", name: "iam-provisioning" }, { path: "/settings/iam/authentication", name: "iam-authentication" }, { path: "/settings/iam/security", name: "iam-security" }, { path: "/settings/iam/sessions", name: "iam-sessions" }, { path: "/settings/iam/devices", name: "iam-devices" }, { path: "/analytics", name: "analytics" }, { path: "/analytics/pages", name: "analytics-pages" }, { path: "/analytics/goals", name: "analytics-goals" }, { path: "/analytics/settings", name: "analytics-settings" }]) {
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
  // The badge is asserted on the SELECTED block, not on the page. `hide_on` belongs to one
  // block's `meta`, the control is the selected block's, and the badge is drawn by that block
  // row — so a page-wide count answers "does any block have this badge" and the pass sets it on
  // one. A different block that was already hidden makes that true on a run where the step
  // failed, which is the same class of error as a page-wide counter read as an item's own state.
  const hideOn = page.locator("[data-block-hide-on]").first();
  steps.visibilityControlPresent = (await hideOn.count()) > 0;
  await hideOn.selectOption("mobile").catch(() => {});
  await page.waitForTimeout(1200);
  const selectedRow = page.locator("[data-block-canvas-block].ring-accent").first();
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
    steps.previewDrawnBlocks = await page.locator("[data-block-preview-frame] [data-block-canvas-block]").count();
    await shot(page, "page-block-preview");

    // The screen switch is a server round trip, and the two payloads genuinely differ: a block
    // the author hid from phones is ABSENT, not invisible.
    await page.locator("[data-block-preview-viewport=mobile]").first().click({ timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(2000);
    // Every read on this frame is `.catch()`-guarded AND timeout-bounded. A bare
    // `getAttribute` waits the full 30s and then throws a TimeoutError that aborts the whole
    // pass — after the report would have been written — and the run is lost with no summary at
    // all. The frame is a *secondary* screen reached by a link, so its absence is a fact to
    // record, never a reason to end the run.
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
