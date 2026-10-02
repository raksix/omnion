#!/usr/bin/env node
// The export panel must show which of the FOUR download refusals applies, before anyone presses.
//
// This checks the screen's source directly, without a browser and without a server, because the
// failure it guards is silent by construction. `GET /deployment/exports/{id}/download` answers a
// bare `410` whose *message* is the only thing that separates four situations with four different
// follow-ups: revoked (ask for a new one), expired (ask for a new one, later), already
// downloaded (you spent it — ask for a new one) and not-ready (wait, or press Produce). A panel
// that knows only "download failed" turns the single-use guarantee into a support ticket, and the
// operator cannot tell whether the file is lost or merely spent.
//
// The panel's answer has to be right BEFORE the press, not derived from the error after it, so:
//   1. every one of the four codes appears as a distinct branch,
//   2. the expiry comparison is against a clock read ONCE per render, not per row — a `Date.now()`
//      inside the row loop lets two rows in one frame disagree about the same instant, and
//   3. where a download cannot work there is no link at all: a control that is present and
//      refuses when pressed is the dead button the request forbids.
//
// Comments are stripped before matching. This file explains the bug in prose, so a comment
// naming a code would satisfy a regex looking for that code's branch — a fix could be described
// here while never being written.
const fs = require("fs");
const path = require("path");

const viewPath = path.join(__dirname, "..", "..", "apps", "admin", "features", "deployment", "exports-view.tsx");
const src = fs.readFileSync(viewPath, "utf8");

const stripComments = (text) =>
  text.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/^[ \t]*\/\/.*$/gm, " ");
const code = stripComments(src);

// The refusal function is the subject. Cut from its declaration to the next top-level `function`
// or the first `export function`, whichever comes first, so a later helper cannot satisfy a check
// aimed at this one.
const fnStart = code.indexOf("function blockedReason(");
if (fnStart < 0) {
  throw new Error(
    "blockedReason() not found in exports-view.tsx — it was renamed or moved, and this gate no " +
      "longer covers the panel's honesty claim. The four refusals are now unmeasured.",
  );
}
const fnEnd = code.indexOf("function statusBadge(", fnStart);
const fn = code.slice(fnStart, fnEnd < 0 ? undefined : fnEnd);

const failures = [];
const check = (name, ok, why) => {
  if (!ok) failures.push(`${name} — ${why}`);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}`);
};

// ---- 1. all four refusals are distinct REACHABLE branches ---------------------------------------
// The four strings are the API's own vocabulary (exports.rs names exactly these), so a rename
// upstream shows up here rather than as a panel that quietly answers the wrong thing.
//
// Each check requires the guard AND the code in the SAME branch, and requires that the guard is
// not a constant. Matching the literal alone is an assertion that cannot fail: the first version
// of this gate was proven on a mutation that rewrote `if (entry.download_count > 0)` as
// `if (false)` and it stayed GREEN, because the unreachable branch still carried
// `code: "downloaded"` in its body. A test that passes on a branch no input can reach measures
// the file, not the panel.
function branchFor(code) {
  // Every `if (...) {` block, non-greedy to its closing brace. The bodies here hold no nested
  // `if`, so the first `}` closes the branch; if that ever stops being true the gate reports a
  // miss rather than silently matching across a boundary.
  //
  // The condition is matched with a manual paren scan rather than `[^)]*`. The expiry guard
  // contains `new Date(entry.expires_at).getTime()` — a `[^)]*` stops at the first `)`, which is
  // the one inside `Date(`, so the pattern matched NO branch at all and reported all three
  // refusals missing on the unmodified file. A gate that is red on correct code is worse than no
  // gate: it teaches the reader to ignore it.
  const re = /if\s*\(/g;
  let match;
  while ((match = re.exec(fn)) !== null) {
    // Walk forward to the paren that closes the condition.
    let depth = 1;
    let index = match.index + match[0].length;
    while (index < fn.length && depth > 0) {
      if (fn[index] === "(") depth += 1;
      else if (fn[index] === ")") depth -= 1;
      index += 1;
    }
    const condition = fn.slice(match.index + match[0].length, index - 1).trim();
    // Past the condition: whitespace, then the block, whose body holds no nested braces.
    const open = fn.indexOf("{", index);
    if (open < 0) continue;
    const close = fn.indexOf("}", open);
    if (close < 0) continue;
    if (fn.slice(index, open).trim() !== "") continue;
    if (fn.slice(open + 1, close).includes(`code: "${code}"`)) {
      return { condition, body: fn.slice(open + 1, close) };
    }
  }
  return null;
}

for (const code of ["revoked", "expired", "downloaded"]) {
  const branch = branchFor(code);
  check(
    `the ${code} refusal is a reachable branch`,
    branch !== null,
    `no branch returns code "${code}" — the panel cannot tell this case from the other three`,
  );
  if (branch) {
    check(
      `the ${code} branch is guarded by the row, not by a constant`,
      !/^(false|true|null|undefined|0|1|""|'')$/.test(branch.condition),
      `the ${code} branch is \`if (${branch.condition})\`, which never varies with the row — it is ` +
        "unreachable in practice and the refusal is never shown",
    );
  }
}
// The fourth has no literal token: it is the catch-all for a status that is not `ready`, and it
// must still carry a code rather than falling through to "no reason".
check(
  "a status that is not ready is refused with a code rather than falling through",
  fn.includes("code: entry.status"),
  "a queued/running/failed export yields no refusal at all — the panel would offer a download that 410s",
);

// ---- 2. expiry is compared against ONE clock per render -----------------------------------------
// `new Date(...).getTime() <= now` where `now` is the render's single reading. A `Date.now()`
// written inside the row map would let row 1 read one instant and row 2 another, and two exports
// expiring in the same second would render as one expired and one ready.
check(
  "expiry is compared against a clock read once, not inside the row loop",
  /new Date\(\s*\w+\.expires_at\s*\)\.getTime\(\)\s*<=\s*now/.test(fn) && !/Date\.now\(\)/.test(fn),
  "blockedReason() reads the clock itself — two rows can disagree about the same instant",
);

// ---- 3. no link where the download cannot work --------------------------------------------------
// The check is structural: the anchor's href must sit behind the same `!blocked` decision that
// suppresses it. Matching `href={` alone would pass on a panel that renders the link regardless.
check(
  "the download link is rendered only when there is no refusal",
  /\{\s*!blocked\s*\?\s*\(\s*<a[\s\S]{0,200}?href=\{exportDownloadUrl/.test(code),
  "the download anchor is not gated on `!blocked` — a control that refuses when pressed is a dead button",
);

// ---- 4. the four states are not colour alone -----------------------------------------------------
// Every state carries a WORD. A status that is only distinguishable by hue fails the acceptance
// rule that drift and refusals must not rely on colour, so the label strings must exist.
for (const label of ["Ready", "Expired", "Revoked", "Failed", "Queued"]) {
  check(
    `the ${label} state has a word, not only a colour`,
    code.includes(`"${label}"`),
    `no "${label}" label — the state is carried by hue alone`,
  );
}

if (failures.length > 0) {
  console.error(`\n${failures.length} export-state check(s) failed:`);
  for (const line of failures) console.error(`  - ${line}`);
  process.exit(1);
}
console.log("\nexport panel: all download states are named before the press");