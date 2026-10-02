#!/usr/bin/env node
/**
 * Omnion QA — refresh docs/qa/QA-LATEST.md from a pass directory.
 *
 *   node scripts/qa/summarize.cjs qa-artifacts/<ts>
 */
"use strict";

const fs = require("fs");
const path = require("path");

const out = path.resolve(process.argv[2] || "");
if (!out || !fs.existsSync(out)) {
  console.error("[summarize] pass directory required");
  process.exit(1);
}

const summary = JSON.parse(fs.readFileSync(path.join(out, "summary.json"), "utf8"));
const visionPath = path.join(out, "findings", "vision.json");
const vision = fs.existsSync(visionPath) ? JSON.parse(fs.readFileSync(visionPath, "utf8")) : { skipped: "not run" };
const visionIssues = Array.isArray(vision.issues) ? vision.issues : [];

/**
 * Coverage is read before anything else, and a filtered pass is labelled in the document's
 * first line rather than in a footer.
 *
 * QA-LATEST.md is the artefact a later tick — or a person — reads to decide whether a REQ may
 * close. If a pass that walked five screens produces the same document as one that walked
 * sixty-one, then the document cannot be used for that decision at all, and worse, it looks
 * authoritative. So the verdict and the scope are stated together, always, and a filtered pass
 * says in its own first line that it is not a build verdict.
 */
const coverage = summary.coverage || null;
const coverageLine = coverage?.filtered
  ? `- **⚠ FILTERED PASS — \`QA_ROUTES=${summary.routeFilter}\`: this is a partial pass, not a build verdict.** ${coverage.routesWalked} of ${coverage.routesTotal} routes walked, ${coverage.depthPassesSkipped} of ${coverage.depthPassesTotal} depth passes not run. The screens measured below are sound; the ones left out are **unmeasured**.`
  : `- Coverage: full pass — ${coverage?.routesWalked ?? summary.counts?.pages ?? 0} routes walked, no depth pass skipped.`;

const doc = [
  "# Omnion QA — latest pass",
  "",
  coverage?.filtered ? "**PARTIAL — filtered pass. Do not read the findings below as a build verdict.**" : "",
  "",
  `- When: ${summary.startedAt || "?"} · artifacts: \`${path.relative(process.cwd(), out)}\``,
  coverageLine,
  `- Interactions: ${summary.counts?.clicks ?? 0} clicks · ${summary.counts?.filled ?? 0} field fills · ${summary.counts?.forms ?? 0} form submissions · ${summary.counts?.screenshots ?? 0} screenshots`,
  `- Console errors: ${summary.counts?.consoleErrors ?? 0} · failed requests: ${summary.counts?.failedRequests ?? 0} · dialogs: ${summary.counts?.dialogs ?? 0}`,
  `- Programmatic findings: ${summary.findings?.length ?? 0} (high ${summary.bySeverity?.high ?? 0} · medium ${summary.bySeverity?.medium ?? 0} · low ${summary.bySeverity?.low ?? 0})`,
  `- Vision issues: ${visionIssues.length}${vision.skipped ? ` (${vision.skipped})` : ""}${vision.failures ? ` · review failures: ${vision.failures}` : ""}`,
  "",
  "## Programmatic findings (high + medium)",
  "",
  ...(summary.findings || []).filter((f) => f.severity !== "low").slice(0, 25).map((f) => `- **[${f.severity}] ${f.kind}** — ${f.detail}`),
  "",
  "## Vision review",
  "",
  ...(visionIssues.length
    ? visionIssues.map((i) => `- **[${i.severity || "low"}] ${i.shot}** — ${i.area || ""}: ${i.evidence || ""}`)
    : ["_No visual defects reported._"]),
  "",
].join("\n");

fs.mkdirSync("docs/qa", { recursive: true });
fs.writeFileSync("docs/qa/QA-LATEST.md", doc);
console.log("docs/qa/QA-LATEST.md updated");
