#!/usr/bin/env node
// The first-run wizard must be WAITED FOR, not waited on with a fixed sleep — and it must
// never be submitted empty.
//
// This checks both properties directly against the harness source, without a browser and
// without a server, because the failure they guard is silent by construction. `runWizard`
// used to `waitForTimeout(900)` and then read `page.url()`: on a cold `next dev` the
// client-side `router.replace("/setup")` from `/login` had not run yet, so the guard saw a
// URL without `/setup`, returned `ran: false`, and the pass carried on into a tenant that
// did not exist. `ensureSignedIn` then reached `/setup` seconds later — with warm modules —
// and its generic `primaryClick` pressed "Create account" with every field still empty,
// which the API correctly refused with 400. Every org-scoped screen below answered 403
// for a tenant nobody created, and the report presented 1,477 high findings as a verdict
// about the product.
//
// Four invariants, one file:
//   1. the wizard entry point POLLS for the redirect instead of sleeping a fixed span,
//   2. a step is only submitted when the form actually carries values, so an unfilled
//      click can never be mistaken for a refused step,
//   3. a wizard that did not run is reported with a REASON, and
//   4. the sign-in after the wizard POLLS for the app shell, for the same reason as (1).
//
// Invariants 3 and 4 came from a run that had already succeeded at everything else:
// `/tmp/w6-pass-t45b.log` seeded owner, organization and site over the API, and then died at
// `FATAL: could not sign in` because the panel was still compiling the route the sign-in click
// triggered. Three consecutive ticks recorded that pass as "queued and never ran" by reading
// `summary.json`'s reason string instead of the rest of the file.
const fs = require("fs");
const path = require("path");

const src = fs.readFileSync(path.join(__dirname, "walkthrough.cjs"), "utf8");
const start = src.indexOf("async function runWizard(");
const end = src.indexOf("async function ensureSignedIn(");
if (start < 0 || end < 0) {
  throw new Error("runWizard() not found — it was moved or renamed; this gate no longer covers it");
}

// Match the CODE, not the prose about the code. These checks read a file whose whole purpose
// is to explain a bug in words, so a comment that says "ran: false" satisfies a regex looking
// for a `ran: false` RETURN, and a fix could be described in a comment while never being
// written. Stripping comments first is what makes each check fail on the property it names.
const stripComments = (text) =>
  text.replace(/\/\*[\s\S]*?\*\//g, " ").replace(/^[ \t]*\/\/.*$/gm, " ");
const body = stripComments(src.slice(start, end));

const failures = [];
const check = (name, ok, why) => {
  if (!ok) failures.push(`${name} — ${why}`);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}`);
};

// ---- 1. the redirect is polled, not slept through -------------------------------------------------
// A single `waitForTimeout(900)` before the `/setup` test is the exact construct that
// produced the silent skip. Polling is what makes the guard ask the question again.
check(
  "wizard waits for the setup redirect rather than sleeping once",
  !/waitForTimeout\(\s*900\s*\)[\s\S]{0,200}?url\.includes\("\/setup"\)/.test(body),
  "a fixed sleep still guards the /setup check — poll until the URL settles instead",
);
check(
  "the wizard polls page.url() in a loop",
  /(for\s*\(\s*let\s+\w+\s*=\s*0;[\s\S]{0,400}?page\.url\(\))/s.test(body),
  "no polling loop over page.url() — the redirect can be checked only once",
);

// ---- 2. an empty step is never submitted -----------------------------------------------------------
// `primaryClick` is the generic submit used by `ensureSignedIn`. Pressing it on an unfilled
// wizard form produces a 400 that says nothing about the platform. The invariant is that a
// *decision* sits between the fill and the click — so match the decision, not its spelling.
const guard = body.match(
  /await\s+fillWizardStep\(page\)([\s\S]{0,700}?)await\s+clickAction\(page\)/,
);
check(
  "the wizard submits only after fillWizardStep reported values",
  guard !== null && /(length\s*[=!<>]=?\s*0|\.filter\(|every\(|some\()/.test(guard[1]),
  "no emptiness decision between filling a step and clicking its action button",
);

// ---- 3. the report says what happened ---------------------------------------------------------------
// A guard that silently returns produced a pass with `ran: false` and no evidence. The
// summary has to distinguish "not a first run" from "the first run never got driven", and it
// has to say which. So the early return has to carry a REASON, not just a flag.
const earlyReturn = body.match(/ran:\s*false[\s\S]{0,160}/);
check(
  "a wizard that did not run is reported with a reason",
  earlyReturn !== null && /(reason|skippedBecause)\s*:/.test(earlyReturn[0]),
  "the early return carries only a flag, so a skipped tenant is indistinguishable from a set-up one",
);
check(
  "the skip is recorded in the report's step log",
  /report\.steps\.push\(\{[^}]*wizard-skipped/.test(body),
  "nothing in the report records that the first run was skipped",
);

// ---- 4. the sign-in is WAITED FOR, not waited on with a fixed sleep -----------------------------
// The same defect class as invariant 1, one function over. `ensureSignedIn` submitted the login
// form and then `waitForTimeout(1200)` before reading `page.url()`. On a cold `next dev` that
// click is what makes Turbopack compile the authenticated route, so 1.2 s is routinely answered
// while the panel is still compiling and the address is still `/login`.
//
// The pass then returned `signedIn: false` and died at `FATAL: could not sign in after wizard` on
// a login that had been accepted. `/tmp/w6-pass-t45b.log` is exactly that run: the seeder had
// created owner, organization and site successfully three lines earlier, and the only thing the
// pass could not do was notice that it was already signed in.
//
// The invariant is a POLL, and it must poll for the app SHELL rather than for the URL — a panel
// that renders `nav[aria-label="Sections"]` is signed in even if the router has not rewritten the
// address yet, and reading only the URL calls that a failure.
// `body` is the slice BETWEEN runWizard and ensureSignedIn, so the sign-in function is not in it —
// it has to be cut out of the whole source, and from the COMMENT-STRIPPED text, or a prose
// mention of `nav[aria-label="Sections"]` in a comment would satisfy the check it is meant to
// falsify. The function's real end is the first line that is exactly `}` at column 0; a lazy
// `\n}` would stop at the first closing brace inside it.
const stripped = stripComments(src);
const signin = stripped.match(/async function ensureSignedIn[\s\S]*?\n\}\n/);
check(
  "the sign-in function is present to check",
  signin !== null,
  "ensureSignedIn could not be located in walkthrough.cjs",
);
const signinBody = signin ? signin[0] : "";
check(
  "the sign-in does not decide from a fixed sleep",
  !/primaryClick\(page\)[\s\S]{0,120}?await page\.waitForTimeout\(\s*1200\s*\)/.test(signinBody),
  "a fixed sleep still guards the post-login check — poll until the shell renders instead",
);
check(
  "the sign-in polls for the app shell, not only for the address changing",
  /nav\[aria-label="Sections"\]/.test(signinBody) && /(for\s*\(\s*let\s+\w+\s*=\s*0)/.test(signinBody),
  "the post-login wait does not poll for the shell — a compiling route looks identical to a refusal",
);
check(
  "a refused sign-in is reported with what the panel said",
  /(refusal|role="alert")/.test(signinBody),
  "a refused sign-in returns false with no reason, so it is indistinguishable from a slow compile",
);

if (failures.length > 0) {
  console.error(`\n${failures.length} wizard gate check(s) failed:`);
  for (const line of failures) console.error(`  - ${line}`);
  process.exit(1);
}
console.log("\nall wizard gate checks passed");