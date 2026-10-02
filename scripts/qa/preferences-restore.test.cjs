#!/usr/bin/env node
// The notification-preferences depth pass must not depend on the row it inherits.
//
// This checks the SOURCE, without a browser and without a server, because the failure it
// guards is silent by construction and takes 50 minutes to surface. The pass restores the
// preferences row at the end so a later pass starts from the defaults — and the restore was a
// `PUT` with no `x-omnion-csrf` header. The API refuses such a write with `csrf_unavailable`
// BEFORE the handler runs, and the call sat inside a `.catch(() => {})`, so a restore that
// changed nothing reported `steps.restored = true`. Nothing was restored, so the next pass
// started from the previous one's leftovers (quiet hours on, digest weekly). The quiet-hours
// step then met a row that already held the values it was about to write, and its blind toggle
// click *unchecked* the box instead of checking it: the fields unmounted, the save button was
// legitimately disabled, and `locator.click` on Save timed out. The fatal named the Save button,
// fifty minutes later, as if the product were broken.
//
// Three invariants, one file:
//   1. the restore carries the CSRF header,
//   2. `restored` is reported from the server's answer rather than hardcoded, and
//   3. the quiet-hours toggle is driven to a KNOWN state instead of clicked blindly, so a row
//      that is already correct cannot uncheck itself.
//
// Match the CODE, not the prose about it: this file's whole job is to explain the bug in
// words, so a comment mentioning `steps.restored = true` would satisfy a regex looking for that
// assignment. Comments are stripped first.
const fs = require("fs");
const path = require("path");

const src = fs.readFileSync(path.join(__dirname, "walkthrough.cjs"), "utf8");
const start = src.indexOf("async function runNotificationSettingsDepth(");
const end = src.indexOf("async function ", start + 10);
if (start < 0 || end < 0) {
  throw new Error("runNotificationSettingsDepth() not found — it was renamed; this gate no longer covers it");
}

const stripComments = (text) =>
  text.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/^[ \t]*\/\/.*$/gm, " ");
const body = stripComments(src.slice(start, end));

const failures = [];
const check = (name, ok, why) => {
  if (!ok) failures.push(`${name} — ${why}`);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}`);
};

// ---- 1. the restore carries the CSRF header --------------------------------------------------
// Scoped to the restore block alone: the rate-limit probe elsewhere in this file already
// sends the header, so a whole-file search would pass on someone else's correct fetch.
const restoreAt = body.indexOf("steps.restored");
const restoreBlock = body.slice(Math.max(0, restoreAt - 3000), restoreAt + 200);
check(
  "the preferences restore sends the x-omnion-csrf header",
  /x-omnion-csrf/.test(restoreBlock),
  "a cookie-authenticated PUT without it is refused with csrf_unavailable before the handler runs",
);
check(
  "the preferences restore reads the omnion_csrf cookie",
  /omnion_csrf=/.test(restoreBlock),
  "the header value is the readable cookie; the panel's own client echoes the same one",
);

// ---- 2. `restored` is answered, not assumed -------------------------------------------------
// The load-bearing half. Even with the header, a `= true` would report success for a server
// that answered 500 or a body that did not parse.
check(
  "`restored` is not a hardcoded true",
  !/steps\.restored\s*=\s*true\b/.test(body),
  "a literal true reports a refused restore as a clean one — the exact shape of the original defect",
);
check(
  "`restored` is compared against the server's status",
  /steps\.restored\s*=\s*\w*[Ss]tatus\s*===\s*200/.test(body),
  "the claim is \"the API accepted the restore\", so it has to be read off the answer",
);

// ---- 3. the quiet-hours toggle is conditional, not blind -------------------------------------
// A blind click on an already-checked box unchecks it: the fields unmount, nothing differs
// from the loaded row, Save is correctly disabled, and the pass dies on the Save button.
const guardAt = body.indexOf('[data-quiet-toggle]").isChecked()');
check(
  "the quiet-hours toggle click is guarded by its current state",
  guardAt >= 0 && /if\s*\(\s*!\s*\w+\s*\)\s*\{/.test(body.slice(guardAt, guardAt + 260)),
  "an unconditional click on a checked box unchecks it and removes the fields this step then fills",
);
check(
  "the toggle's state is read immediately before the guarded click",
  guardAt >= 0 && body.indexOf('[data-quiet-toggle]").click(') > guardAt,
  "reading the state and the click have to describe the same control; a read of some other\n   element would satisfy the check above while leaving the click blind",
);

// ---- 4. a refused save cannot be silent -------------------------------------------------------
// `steps.quietIsDirty` is what distinguishes "the button was disabled because the screen was
// never dirtied" from "the save is still in flight" — the two look identical from a timeout.
check(
  "the dirty state is asserted before clicking Save",
  /steps\.quietIsDirty\s*=\s*await/.test(body),
  "without it a disabled Save button is indistinguishable from a hung request",
);

if (failures.length) {
  console.error(`\n${failures.length} preferences-restore check(s) failed:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log("\nall preferences-restore checks passed");
