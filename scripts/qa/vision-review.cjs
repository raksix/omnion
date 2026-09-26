#!/usr/bin/env node
/**
 * Omnion QA — vision review of the walkthrough screenshots.
 *
 * Sends every screenshot (page-level first, then interactions and mobile) to a vision model and
 * writes back the visual defects it reports. Never fails the pipeline: when the model is
 * unreachable (missing key, quota, timeout) it records the skip and exits 0.
 *
 *   --dir qa-artifacts/<ts>   walkthrough output directory (required)
 *   --max 14                  ceiling on images per pass
 *
 * Output: <dir>/findings/vision.json (+ a section appended to <dir>/report.md)
 */
"use strict";

const fs = require("fs");
const path = require("path");

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i !== -1 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const DIR = path.resolve(arg("dir", ""));
const MAX = Number(arg("max", "14"));
if (!DIR || !fs.existsSync(DIR)) {
  console.error("[vision] --dir is required and must exist");
  process.exit(1);
}
const OUT = path.join(DIR, "findings");
fs.mkdirSync(OUT, { recursive: true });

// ---------------------------------------------------------------- credentials

function readConfig() {
  try {
    const cfg = fs.readFileSync("/root/.hermes/config.yaml", "utf8");
    const pick = (key) => {
      const m = cfg.match(new RegExp(`${key}\\s*:\\s*["']?([^\\s"']+)`));
      return m ? m[1] : "";
    };
    return {
      key: pick("VISION_MCP_API_KEY"),
      base: pick("VISION_MCP_BASE_URL"),
      model: pick("VISION_MCP_MODEL"),
    };
  } catch {
    return { key: "", base: "", model: "" };
  }
}

const cfg = readConfig();
const API_KEY = process.env.VISION_MCP_API_KEY || cfg.key;
const BASE = (process.env.VISION_MCP_BASE_URL || cfg.base || "https://api.commandcode.ai/provider/v1").replace(/\/+$/, "");
const MODEL = process.env.VISION_MCP_MODEL || cfg.model || "deepseek/deepseek-v4.1-flash";
const ENDPOINT = `${BASE}/chat/completions`;

const PROMPT = `You are a strict visual QA reviewer for a web admin panel (screenshots at 1440px desktop or 390px mobile).
Report ONLY real, visible defects. Look for:
- text clipped, cut off, truncated mid-word, or overflowing its container
- overlapping elements, text over text, buttons colliding
- broken layout: misaligned columns, uneven card heights that look broken, elements outside their box
- invisible or very low-contrast text (light grey on white, white on light)
- empty boxes/cards/sections where content is clearly expected
- broken or missing images (placeholder icons, alt text, grey rectangles)
- horizontal overflow, elements pushed off the right edge, or a horizontal scrollbar
- raw untranslated keys like "pages.title", "{{count}}", "object Object", unresolved placeholders
- obvious dev artifacts (debug borders, "TODO", "lorem ipsum")
DO NOT report: subjective style preferences, spacing taste, missing features, English copy style, or anything you cannot see. If the screenshot looks correct, return an empty list.
Reply with STRICT JSON only: {"issues":[{"severity":"high|medium|low","area":"<element or region>","evidence":"<what you see>","fix":"<short suggestion>"}]}`;

// ---------------------------------------------------------------- pick shots

function collect() {
  const files = fs.readdirSync(path.join(DIR, "shots")).filter((f) => f.endsWith(".png"));
  const rank = (f) => {
    if (f.startsWith("page-")) return 0;
    if (f.startsWith("mobile-")) return 1;
    if (f.startsWith("web-")) return 2;
    if (f.startsWith("form-")) return 3;
    if (f.startsWith("click-")) return 4;
    return 5;
  };
  return files
    .sort((a, b) => rank(a) - rank(b) || a.localeCompare(b))
    .slice(0, MAX)
    .map((f) => path.join(DIR, "shots", f));
}

// ---------------------------------------------------------------- request

async function reviewOne(file) {
  const b64 = fs.readFileSync(file).toString("base64");
  const body = {
    model: MODEL,
    max_tokens: 2200,
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: PROMPT },
          { type: "image_url", image_url: { url: `data:image/png;base64,${b64}` } },
        ],
      },
    ],
  };
  const res = await fetch(ENDPOINT, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${API_KEY}` },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(120000),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`HTTP ${res.status}: ${text.slice(0, 300)}`);
  let content = "";
  try {
    const json = JSON.parse(text);
    content = json.choices?.[0]?.message?.content ?? "";
    if (!content && json.choices?.[0]?.message?.reasoning_content) content = "";
  } catch {
    content = text;
  }
  return content;
}

function parseIssues(content) {
  if (!content || !content.trim()) return { issues: [], empty: true };
  const cleaned = content.replace(/```json/gi, "").replace(/```/g, "").trim();
  const start = cleaned.indexOf("{");
  const end = cleaned.lastIndexOf("}");
  if (start === -1 || end === -1) return { issues: [], raw: cleaned.slice(0, 600) };
  try {
    const parsed = JSON.parse(cleaned.slice(start, end + 1));
    const issues = Array.isArray(parsed.issues) ? parsed.issues : [];
    return { issues };
  } catch {
    return { issues: [], raw: cleaned.slice(0, 600) };
  }
}

async function main() {
  if (!API_KEY) {
    const out = { skipped: "no vision API key found (env VISION_MCP_API_KEY or config.yaml)" };
    fs.writeFileSync(path.join(OUT, "vision.json"), JSON.stringify(out, null, 2));
    console.log("[vision] skipped: no API key");
    return;
  }
  const files = collect();
  console.log(`[vision] reviewing ${files.length} screenshot(s) with ${MODEL}`);
  const results = [];
  let failures = 0;
  for (const f of files) {
    const name = path.basename(f, ".png");
    let content = "";
    let error = null;
    for (let attempt = 1; attempt <= 2; attempt += 1) {
      try {
        content = await reviewOne(f);
        error = null;
        break;
      } catch (err) {
        error = String(err.message || err).slice(0, 300);
        await new Promise((r) => setTimeout(r, 1500 * attempt));
      }
    }
    if (error) {
      failures += 1;
      results.push({ shot: name, error });
      console.log(`[vision] ${name}: FAILED (${error.slice(0, 120)})`);
      continue;
    }
    const parsed = parseIssues(content);
    const issues = (parsed.issues || []).map((i) => ({ ...i, shot: name }));
    results.push({ shot: name, issues, raw: parsed.raw });
    console.log(`[vision] ${name}: ${issues.length} issue(s)`);
    await new Promise((r) => setTimeout(r, 400));
  }

  const all = results.flatMap((r) => r.issues || []);
  const bySeverity = { high: 0, medium: 0, low: 0 };
  for (const i of all) bySeverity[String(i.severity || "low").toLowerCase()] = (bySeverity[String(i.severity || "low").toLowerCase()] || 0) + 1;
  const payload = { model: MODEL, endpoint: ENDPOINT, reviewed: results.length - failures, failures, bySeverity, issues: all, results };
  fs.writeFileSync(path.join(OUT, "vision.json"), JSON.stringify(payload, null, 2));

  const md = [];
  md.push("");
  md.push(`## Vision review — ${all.length} issue(s) (high ${bySeverity.high || 0} · medium ${bySeverity.medium || 0} · low ${bySeverity.low || 0})`);
  md.push("");
  if (!all.length) {
    md.push(failures ? `_${failures} screenshot(s) could not be reviewed._` : "_No visual defects reported._");
  }
  for (const i of all) {
    md.push(`- **[${i.severity || "low"}] ${i.shot}** — ${i.area || ""}: ${i.evidence || ""}${i.fix ? ` → _${i.fix}_` : ""}`);
  }
  md.push("");
  fs.appendFileSync(path.join(DIR, "report.md"), md.join("\n"));

  console.log(`VISION_ISSUES=${all.length} VISION_HIGH=${bySeverity.high || 0} VISION_FAILURES=${failures}`);
}

main().catch((err) => {
  console.error("[vision] unexpected failure:", err);
  try {
    fs.writeFileSync(path.join(OUT, "vision.json"), JSON.stringify({ fatal: String(err) }, null, 2));
  } catch {
    /* ignore */
  }
  process.exit(0); // never fail the pipeline for the review step
});
