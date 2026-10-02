#!/usr/bin/env node
/**
 * scripts/qa/probe-expected-refusals.cjs — an allowance that cannot match is worse than none.
 *
 * The roll-up's contract, in one sentence: **a refusal the pass provokes on purpose is an
 * assertion, not a defect**, so it is reported under `expectedRefusals` instead of arriving as a
 * high finding. Two halves have to hold for that contract to be true, and for three ticks only
 * the first did.
 *
 * **1. The net matcher could not match what the file registers.** It accepted `[401, 403]` and
 * nothing else — the shape an *authorisation* gate refuses with — while the file's most-registered
 * deliberate refusals are not authorisation at all. A two-tab conflict is a **409**, a body the
 * server is right to reject is a **422**, a loop the pass left open on purpose is a **400**, and a
 * read of a collection that is legitimately empty is a **404**. None could be excused by the
 * allowance the pass had registered for it, so every one arrived as a **high finding**: tick 78
 * read "19 high findings" and guessed "the probe's own refusals" — with `expectedRefusals` empty
 * beside them, so nothing in the report contradicted it and nothing would have in any of the three
 * ticks that ran afterwards.
 *
 * The console half was broken in the opposite direction and would have been made *worse* by the
 * obvious fix: a line was excused on the status alone (`/status of 40[13]/`), so whichever
 * allowance happened to be live swallowed every 401/403/404-shaped line in the rest of the
 * session. Widening the status list without scoping the match by URL would have turned "charges the
 * pass for its own refusals" into "hides everybody's 403".
 *
 * **2. Nothing said an allowance went unused.** `summary.json` carried `expectedRefusals`, which is
 * the list of refusals it *claimed*. An allowance that matched nothing left no trace, so a pass
 * charged for its own deliberate refusals produced **the same report** as a pass that provoked
 * none. The empty list was being read as evidence for three ticks and it was evidence of nothing.
 *
 * ## How this gate is built, and why
 *
 * Two kinds of rule, and the split is deliberate:
 *
 * - **Behavioural** (sections 1–4) run the matcher against the shapes passes really register. They
 *   transcribe the roll-up's rule rather than importing it, because `walkthrough.cjs` is a script
 *   and importing one RUNS the whole pass. The transcription's job is to state *intent*.
 * - **Structural** (section 5) read the real source and assert the two decisions the matcher makes.
 *   Their job is to catch a regression in the *shipped* rule, which is what the mutation block in
 *   section 6 proves they can do.
 *
 * Section 6 is what makes the file non-vacuous: each mutation re-introduces a shipped defect into a
 * temp COPY of the source and re-runs this gate against it, requiring a **named** rule to go red.
 * A mutation that leaves the gate green — or that turns it red for an unrelated reason — is a gate
 * that cannot see the bug it was written for, and both are reported as failures.
 *
 * Run: `node scripts/qa/probe-expected-refusals.cjs` (exit 0 = pass, 1 = fail).
 */
const fs = require("fs");
const path = require("path");

// `QA_WALKTHROUGH_SRC` has exactly one caller: the mutation block, which points it at a temp copy
// of the source with a defect re-introduced. Unset, it is the walkthrough next to this file — the
// file the pass actually runs.
const SRC = process.env.QA_WALKTHROUGH_SRC || path.join(__dirname, "walkthrough.cjs");

// The mutation block re-executes this file, so without this the child would spawn its own
// mutations, whose children would spawn theirs, and a green run would be indistinguishable from an
// exponential one. It is also the reason a mutation's red could not be traced: in the first draft
// the child reported red on its own *nested* mutation block rather than on the defect under test.
const IS_MUTANT = process.env.QA_REFUSAL_MUTANT === "1";

let pass = 0;
let fail = 0;
const failures = [];

function check(name, ok, detail = "") {
  if (ok) {
    pass++;
    console.log(`  ok ${pass} - ${name}`);
  } else {
    fail++;
    failures.push(`${name}${detail ? ` (${detail})` : ""}`);
    console.log(`  not ok ${fail} - ${name}${detail ? ` (${detail})` : ""}`);
  }
}

// --------------------------------------------------------------------------------------------
// The matcher, transcribed from the roll-up.
//
// The net half includes the two lines the caller writes AFTER it finds a match: an allowance is
// single-use only because the loop marks it, and transcribing the predicate without the effect made
// "one allowance claims exactly one refusal" fail for a reason that had nothing to do with the
// status list — the exact shape of a gate that is wrong in a shape the reader will believe.
// --------------------------------------------------------------------------------------------

/** The statuses a refusal allowance may cover. Mirrors `REFUSAL_STATUSES` in the source. */
const ALLOWED = [400, 401, 403, 404, 409, 422, 428];
const allowedStatuses = [...ALLOWED];

function claimNet(expectedRefusals, index, failure) {
  const found = expectedRefusals.find(
    (entry) =>
      !entry.claimedNet &&
      index >= entry.netFrom &&
      String(failure.url || "").includes(entry.match) &&
      allowedStatuses.includes(failure.status),
  );
  if (found) found.claimedNet = true;
  return found;
}

function claimConsole(expectedRefusals, index, line) {
  const statusNamed = String(line.text).match(/status(?: of)?\s+(\d{3})/i)?.[1] ?? null;
  if (statusNamed === null || !allowedStatuses.includes(Number(statusNamed))) return null;
  return (
    expectedRefusals.find(
      (entry) =>
        !entry.claimedConsole &&
        index >= entry.consoleFrom &&
        (entry.consoleMatch
          ? String(line.url || "").includes(entry.consoleMatch)
          : String(line.text).includes(entry.match)),
    ) ?? null
  );
}

/** The shape `expectRefusal` produces — including the console fragment it derives. */
function allowance(match, reason, netFrom = 0, consoleFrom = 0, consoleMatch) {
  return {
    match,
    reason,
    // The source derives this: a caller registering a FULL URL gets that URL as its console
    // fragment, and a caller registering a bare path fragment gets none and falls back to text.
    consoleMatch: consoleMatch ?? (match.startsWith("http") ? match : null),
    netFrom,
    consoleFrom,
    claimedNet: false,
    claimedConsole: false,
  };
}

// The form every workflow caller actually uses: a full URL, so the console half has a URL to match.
const WF = "http://127.0.0.1:3102/api/v1/workflows/";

// ---- 1. The statuses a pass really registers -------------------------------------------------

for (const status of [409, 422, 400, 404, 403, 401, 428]) {
  const refusals = [allowance(WF, `a deliberate ${status}`)];
  const claimed = claimNet(refusals, 1, { url: `${WF}abc/graph`, status });
  check(`a deliberate ${status} is an assertion, not a finding`, claimed !== undefined);
}
{
  // The one status that must stay out, and the one that makes the list a list rather than "any
  // error": a crash is the product failing, and no act the pass performed makes it expected.
  const refusals = [allowance(WF, "anything at all")];
  const claimed = claimNet(refusals, 0, { url: `${WF}abc`, status: 500 });
  check("a 500 is never excused by an allowance", claimed === undefined);
}
{
  // 502/503 are the same argument for a downstream dependency.
  const refusals = [allowance(WF, "anything at all")];
  const claimed = claimNet(refusals, 0, { url: `${WF}abc`, status: 503 });
  check("a 503 is never excused by an allowance", claimed === undefined);
}

// ---- 2. An allowance is single-use, ordered and scoped ---------------------------------------

{
  const refusals = [allowance(WF, "one allowance, one refusal")];
  const first = claimNet(refusals, 0, { url: `${WF}a/graph`, status: 409 });
  const second = claimNet(refusals, 1, { url: `${WF}b/graph`, status: 409 });
  check("one allowance claims exactly one refusal", first !== undefined && second === undefined);
}
{
  // A refusal that arrived BEFORE the allowance was registered cannot be excused by it — otherwise
  // an allowance registered at the end of a pass would sweep up the whole session.
  const refusals = [allowance(WF, "registered after the fact", 5)];
  const claimed = claimNet(refusals, 2, { url: `${WF}a/graph`, status: 409 });
  check("an allowance cannot excuse a refusal that arrived before it", claimed === undefined);
}
{
  const refusals = [allowance(WF, "only the graph route")];
  const claimed = claimNet(refusals, 0, { url: "http://127.0.0.1:3102/api/v1/backups/run", status: 409 });
  check("an allowance is scoped to its own URL fragment", claimed === undefined);
}

// ---- 3. The console half keys on the URL, not only on the status -----------------------------

{
  const refusals = [allowance(WF, "the stale version is refused")];
  const unrelated = claimConsole(refusals, 4, {
    url: "http://127.0.0.1:3102/api/v1/roles",
    text: "Failed to load resource: the server responded with a status of 403",
  });
  check("a console 403 on ANOTHER route is not excused by a workflows allowance", unrelated === null);
}
{
  const refusals = [allowance(WF, "the stale version is refused")];
  const own = claimConsole(refusals, 4, {
    url: `${WF}abc/graph`,
    text: "Failed to load resource: the server responded with a status of 409",
  });
  check("a console line naming the allowance's own route and status IS excused", own !== null);
}
{
  // The status has to come from somewhere: a console line about an allowance's own URL carrying no
  // refusal status is not evidence of a refusal.
  const refusals = [allowance(WF, "the stale version is refused")];
  const chatty = claimConsole(refusals, 4, {
    url: `${WF}abc/graph`,
    text: "Failed to load resource: the server responded with a status of 200",
  });
  check("a console line with no refusal status is not excused", chatty === null);
}
{
  // The callers that register a bare path fragment (`"passkeys/"`, `"notifications/emit"`) have no
  // URL to match on, and they are why the console half falls back to the line's own TEXT. Without
  // the fallback their deliberate refusals are findings — the same bug, one caller narrower.
  const refusals = [allowance("passkeys/", "a factor removal the panel must re-prove")];
  const own = claimConsole(refusals, 2, {
    url: "http://127.0.0.1:3102/api/v1/auth/webauthn/passkeys/abc",
    text: "Failed to remove the factor: passkeys/ refused with a status of 403",
  });
  check("a bare-fragment allowance falls back to matching the line's own text", own !== null);
}

// ---- 4. The report must say an allowance went unused -----------------------------------------

{
  const allowances = [allowance(WF, "a refusal that never arrived")];
  const unused = allowances
    .filter((entry) => !entry.claimedNet && !entry.claimedConsole)
    .map((entry) => ({ match: entry.match, reason: entry.reason }));
  check("an allowance that matched nothing is reported as unused", unused.length === 1);
  check(
    "an unused allowance appears in the report alongside the claimed ones",
    unused[0]?.reason === "a refusal that never arrived",
  );
}
{
  // The claimed half must be excluded, or every allowance is reported twice and "unused" stops
  // meaning unused.
  const allowances = [allowance(WF, "a refusal that arrived"), allowance(`${WF}../roles/`, "a refusal that never arrived")];
  claimNet(allowances, 0, { url: `${WF}a`, status: 409 });
  const unused = allowances
    .filter((entry) => !entry.claimedNet && !entry.claimedConsole)
    .map((entry) => entry.reason);
  check(
    "an allowance that DID claim is not reported as unused",
    unused.length === 1 && unused[0] === "a refusal that never arrived",
  );
}

// ---- 5. The SHIPPED source makes the two decisions this gate describes ----------------------
//
// The behavioural rules above test a transcription, and a transcription can be right while the
// source is wrong — which is precisely the failure mode this file exists to prevent. So these rules
// read `walkthrough.cjs` itself and assert the two decisions the roll-up actually makes.

const src = fs.readFileSync(SRC, "utf8");
{
  // Slice the console loop and the net loop by their own anchors rather than trying to parse the
  // file: the earlier `probe-pass-scope.cjs` learned that brace counting cannot lex a regex literal,
  // and a parser that cannot read its input must say so rather than report findings it distrusts.
  const consoleLoopStart = src.indexOf("for (const [index, f] of consoleLog.entries())");
  const netLoopStart = src.indexOf("for (const [index, n] of netFailures.entries())");
  check(
    "the roll-up has both refusal loops where this gate expects them",
    consoleLoopStart > 0 && netLoopStart > consoleLoopStart,
    `console=${consoleLoopStart} net=${netLoopStart}`,
  );
  const consoleLoop = src.slice(consoleLoopStart, netLoopStart);
  const netLoop = src.slice(netLoopStart, netLoopStart + 900);

  // (a) The status list exists, is the one this gate transcribes, and excludes every 5xx.
  const listMatch = src.match(/REFUSAL_STATUSES\s*=\s*\[([^\]]*)\]/);
  check("the source declares the status list this gate tests", listMatch !== null);
  if (listMatch) {
    const declared = listMatch[1]
      .split(",")
      .map((part) => Number(part.trim()))
      .filter((n) => Number.isFinite(n));
    check(
      "the gate's status list and the source's agree",
      declared.length === ALLOWED.length && declared.every((n, i) => n === ALLOWED[i]),
      `source=${JSON.stringify(declared)} gate=${JSON.stringify(ALLOWED)}`,
    );
    check(
      "no 5xx is excusable in the source's list",
      declared.every((n) => n < 500),
      JSON.stringify(declared.filter((n) => n >= 500)),
    );
  }

  // (b) The net loop takes its statuses from that list. A hardcoded array here is the shipped
  // defect wearing a different spelling, so the rule names the list rather than the shape.
  check(
    "the net matcher takes its statuses from REFUSAL_STATUSES",
    /REFUSAL_STATUSES\.includes\(n\.status\)/.test(netLoop) &&
      !/\[\s*40[13]\s*,\s*40[13]\s*\]\.includes/.test(netLoop),
    "the net loop decides on a hardcoded status list again",
  );

  // (c) The console loop reads the status out of the line AND keys the match on the URL.
  check(
    "the console matcher reads the status out of the line",
    /statusNamed\s*=\s*String\(f\.text\)\.match/.test(consoleLoop),
    "the console matcher tests a status with a regex over the whole line instead of reading it",
  );
  check(
    "the console matcher keys on the URL, not only on the status",
    /entry\.consoleMatch/.test(consoleLoop),
    "a live allowance again swallows any 401/403/404 in the session",
  );
  check(
    "the console matcher gates on REFUSAL_STATUSES too",
    /REFUSAL_STATUSES\.includes\(Number\(statusNamed\)\)/.test(consoleLoop),
  );

  // (d) `expectRefusal` derives the console fragment, so a URL-registering caller gets one.
  check(
    "expectRefusal derives a console fragment for a URL-registering caller",
    /consoleMatch:\s*consoleMatch\s*\?\?\s*\(match\.startsWith\("http"\)/.test(src),
  );

  // (e) The report carries the unused half — the shape whose absence made this bug invisible.
  //
  // Both halves are asserted, because `unusedRefusals,` alone is satisfied by a report that writes
  // an empty array, and that IS the defect: the key present, the computation gone. The mutation
  // below changes exactly that and the rule has to notice, so it names the SOURCE of the list.
  check(
    "summary.json reports the unused allowances",
    /const unusedRefusals = expectedRefusals\s*\n?\s*\.filter\(\(entry\) => !entry\.claimedNet && !entry\.claimedConsole\)/.test(
      src,
    ) && /unusedRefusals,/.test(src) && /refusalsUnused:\s*unusedRefusals\.length/.test(src),
    "the report shape that hid the bug is back",
  );
}

// ---- 6. Proven against MUTATED copies of the real source -------------------------------------

if (!IS_MUTANT) {
  const realSrc = fs.readFileSync(SRC, "utf8");
  const mutations = [
    {
      name: "the net matcher narrowed back to a hardcoded [401, 403]",
      why: "a deliberate 409 can no longer be excused, so the pass is charged for its own refusal",
      apply: (s) => s.replace("REFUSAL_STATUSES.includes(n.status)", "[401, 403].includes(n.status)"),
      // The rule that must go red is the STRUCTURAL one — the behavioural rules test the
      // transcription, which the mutation does not touch.
      expectRule: /net matcher takes its statuses from REFUSAL_STATUSES/,
    },
    {
      name: "the console matcher deciding on the status alone",
      why: "whichever allowance is live swallows every 401/403/404 in the rest of the session",
      // The ternary, replaced by a bare `true` — the SHIPPED defect verbatim, which is what makes
      // this mutation worth having. Two earlier versions failed here and both failures are the
      // lesson: one deleted the `statusNamed` declaration (leaving dangling references, so the
      // mutant threw before any rule could decide — the tick-81 mistake), and one matched
      // `entry =>` when the source writes `(entry) =>` inside a `.find(...)` call. A mutation
      // anchors on the STABLE inner text of the decision, not on the punctuation around it.
      apply: (s) =>
        s.replace(
          /\(entry\.consoleMatch\s*\n\s*\?\s*String\(f\.url \|\| ""\)\.includes\(entry\.consoleMatch\)\s*\n\s*:\s*String\(f\.text\)\.includes\(entry\.match\)\)/,
          "true",
        ),
      expectRule: /console matcher keys on the URL/,
    },
    {
      name: "unused allowances dropped from the report",
      why: "a matcher that stopped matching leaves an empty list, which reads as 'no refusals'",
      // `expectedRefusals` → a literal empty list: the report computes the right SHAPE from the
      // wrong SOURCE, which is why it has to be matched on the identifier rather than on the
      // `.filter` call that follows it (a newline in the chain broke an earlier version of this).
      apply: (s) => s.replace("const unusedRefusals = expectedRefusals", "const unusedRefusals = []"),
      expectRule: /summary\.json reports the unused allowances/,
    },
    {
      name: "a 500 added to the excusable list",
      why: "the allowance stops being a licence for a refusal and becomes a switch that hides crashes",
      apply: (s) => s.replace("const REFUSAL_STATUSES = [400, 401, 403, 404, 409, 422, 428];", "const REFUSAL_STATUSES = [400, 401, 403, 404, 409, 422, 428, 500];"),
      expectRule: /no 5xx is excusable|status list and the source's agree/,
    },
  ];

  const { execFileSync } = require("child_process");

  for (const mutation of mutations) {
    const mutated = mutation.apply(realSrc);
    // The gate must be able to say "I could not build the mutant" rather than report a red that
    // came from somewhere else. An `apply()` that no longer matches is a stale gate, and a stale
    // gate that silently passes is the bug this file was written for.
    if (mutated === realSrc) {
      check(`mutation "${mutation.name}" still applies to the source`, false, "apply() matched nothing — this gate is stale");
      continue;
    }
    const tmp = path.join(__dirname, `.mutant-${process.pid}.cjs`);
    fs.writeFileSync(tmp, mutated);
    let out = "";
    let code = 0;
    try {
      out = execFileSync("node", [__filename], {
        // The child must not run the mutation block, or its red would come from a nested
        // mutation rather than from the defect under test.
        env: { ...process.env, QA_REFUSAL_MUTANT: "1", QA_WALKTHROUGH_SRC: tmp },
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      });
    } catch (err) {
      code = err.status ?? 1;
      out = `${err.stdout ?? ""}${err.stderr ?? ""}`;
    } finally {
      fs.unlinkSync(tmp);
    }
    check(
      `the gate is RED on: ${mutation.name}`,
      code !== 0,
      `exit=${code} — the gate passed a source with the defect re-introduced`,
    );
    check(
      `…naming the rule that caught it (${mutation.why})`,
      new RegExp(`not ok \\d+ - .*${mutation.expectRule.source}`, "i").test(out),
      `no "not ok" line for ${mutation.expectRule.source}`,
    );
  }

  // And the gate must be GREEN on the source as shipped, or every mutation above is proving that
  // the gate is red on everything.
  check("the gate is GREEN on the unmutated source", fail === 0, `${fail} rule(s) failed on the real file`);
}

// -------------------------------------------------------------------------------------------------

console.log(`\n${pass} passed, ${fail} failed${IS_MUTANT ? " (mutant run — structural rules only)" : ""}`);
if (fail) {
  console.log(
    "\nAn allowance that cannot match is not a safety net, it is a rule that reports its own refusals as defects:",
  );
  for (const f of failures) console.log(`  - ${f}`);
  process.exit(1);
}
process.exit(0);
