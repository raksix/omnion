#!/usr/bin/env node
/**
 * scripts/qa/probe-edge-delete-claim.cjs — `hit` is the one field in the edge-delete note that
 * cannot be true, and it is the field the criterion's reasoning leans on.
 *
 * **The defect.** `edgeHit` starts `false` and is assigned `true` immediately after
 * `mouse.move / mouse.down / mouse.up`, in a block whose only condition is `if (edgeScreenPoint)` —
 * that is, *a point was measured at all*. So the note's `hit: true` means "the probe moved the mouse
 * and pressed it", not "the press landed on the edge". The press is followed by a 500 ms wait and a
 * selection read, and the selection is what the row actually gates on (`edgeSelected`), so nothing
 * downstream is misled — the row still goes red when the product is dead.
 *
 * **Why that is still worth a tick.** Ticks 78 and 79 each spent themselves on this one row's
 * `selected: false`, and both readings it wrong is exactly what this file forbids:
 *
 *   * tick 78 read `hit: true, onEdge: true` and treated it as *the click was on the edge*;
 *   * tick 79 read `blockedBy: "text"` and had to reason about it with `onEdge`'s help.
 *
 * `hit` is neither, and it reads like the first thing. A future row that gates on `hit` — the
 * obvious way to write "the gesture works", and the way this file's own prose invites it — passes on
 * a probe that pressed the desk. `onEdge` is the browser's hit test at the measured point and is the
 * only one of the three that means what its name says; `blockedBy` names the winner of that test.
 *
 * So the rule is narrow and mechanical: **`hit` may only be set inside a branch whose condition is a
 * measurement of the point, and the note must carry the measurement beside it.** `hit` is not
 * deleted, because a row that reports *what the probe did* is useful — it is demoted from a claim
 * about the product to a fact about the instrument, and the fact beside it is what makes it read
 * correctly.
 *
 * Properties, each proven red against a mutated copy of the source:
 *
 *   1. `edgeHit` is not assigned `true` on an unconditional path — the assignment sits inside a
 *      branch guarded by the hit-test measurement (`edgeScreenPoint.onEdge`), so a point that
 *      resolved on the desk reports `hit: false`;
 *   2. both `edge-delete` notes carry the hit-test evidence (`onEdge` / `blockedBy` / `inViewport`)
 *      next to `hit`, so the number is never the only thing a reader has;
 *   3. the gate is GREEN on the unmutated source.
 *
 * Run: `node scripts/qa/probe-edge-delete-claim.cjs` (exit 0 = pass, 1 = fail).
 * Mutant: `QA_EDGE_MUTANT=<n>` runs only mutation n, for building the table above.
 */
const fs = require("fs");
const path = require("path");

const SRC = process.env.QA_WALKTHROUGH_SRC || path.join(__dirname, "walkthrough.cjs");
const MUTANT = process.env.QA_EDGE_MUTANT || "";

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

let src = fs.readFileSync(SRC, "utf8");

// ---- mutations -------------------------------------------------------------------------------
//
// Each mutation produces a source that SHOULD fail, so that a green run on it means the rule is not
// reading what it claims to read. Every mutation touches exactly one property.

if (MUTANT === "1") {
  // Put the assignment back where it was: the bare constant, after the press.
  //
  // **This regex was wrong twice before it worked.** The first version matched
  // `const edgeHit = (?:let|const)…` — a shape the declaration has not had since the fix, so it
  // silently matched nothing and MUTANT 1 came back 15/15 GREEN on a source that had reverted the
  // whole defect. A mutation that does not apply is worse than no mutation: it is a green line in
  // the table that certifies nothing. The pattern is now anchored to the assignment it must change
  // and the file asserts below that the replacement actually changed the source.
  src = src.replace(
    /edgeHit = Boolean\(edgeScreenPoint\.onEdge\);/,
    "edgeHit = true;",
  );
} else if (MUTANT === "2") {
  // Drop the evidence fields from the failure note: the number survives, the reading does not.
  src = src.replace(/\n\s+onEdge: edgeScreenPoint\?\.onEdge \?\? null,/, "");
  src = src.replace(/\n\s+blockedBy: edgeScreenPoint\?\.blockedBy \?\? null,/, "");
  src = src.replace(/\n\s+inViewport: edgeScreenPoint\?\.inViewport \?\? null,/, "");
}

// ---- 0. the row is still there ----------------------------------------------------------------

if (MUTANT) {
  // A mutation that silently fails to apply is the one that hurts: it reports the property proven
  // on a source that still contains the defect. Verified by re-running the same expression and
  // comparing.
  const before = fs.readFileSync(SRC, "utf8");
  check("the mutation actually changed the source", before !== src, `MUTANT=${MUTANT} matched nothing`);
}

const hitDecl = src.indexOf("edgeHit");
check("the edge-delete row still declares `edgeHit`", hitDecl >= 0, `at ${hitDecl}`);
// `edge-delete-undo` starts with `edge-delete`, so the two names are matched separately: an
// alternation that does not exclude the longer name counts the undo note twice and a gate that
// says "both notes are present" is then satisfied by one note printed once.
const noteSites = [
  ...[...src.matchAll(/step: "edge-delete"/g)].map((m) => ({ name: "edge-delete", index: m.index })),
  ...[...src.matchAll(/step: "edge-delete-undo"/g)].map((m) => ({ name: "edge-delete-undo", index: m.index })),
];
// THREE notes, not two: the selected branch, the undo that follows it, and the miss branch that
// runs when no edge could be selected. An earlier version of this rule expected two and read the
// miss branch as a duplicate -- the same mistake in reverse, and the direction that hides a real
// finding: if the miss branch ever stopped recording `onEdge`, a gate that counted two notes would
// have measured the branch that only runs when things are working.
const bodyOf = (index) => {
  const open = src.indexOf("{", src.lastIndexOf("note(", index));
  // The note object ends at the matching close brace. A brace counter is used rather than a fixed
  // window because these notes are formatted at different widths and one of them has a nested
  // object literal inside a template.
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    const ch = src[i];
    if (ch === "{") depth++;
    else if (ch === "}") {
      depth--;
      if (depth === 0) return src.slice(open, i);
    }
  }
  return "";
};

const hasSelected = noteSites.some((n) => n.name === "edge-delete" && /removed:/.test(bodyOf(n.index)));
const hasMiss = noteSites.some((n) => n.name === "edge-delete" && /selected: false/.test(bodyOf(n.index)));
const hasUndo = noteSites.some((n) => n.name === "edge-delete-undo");
check(
  "the row has a selected-branch note, a miss-branch note and an undo note",
  hasSelected && hasMiss && hasUndo,
  `selected=${hasSelected} miss=${hasMiss} undo=${hasUndo}`,
);

// ---- 1. `hit` is DERIVED from the hit test, not from the probe having pressed the mouse -------
//
// **The rule changed shape once, and the change is the interesting part.** The first version of
// this gate demanded the press be *guarded* by `onEdge`. That is the wrong shape: guarding the
// press would mean a probe that never presses on a point it already knows missed the curve, which
// is worse than useless -- the miss branch's whole job is to report what happened when the press
// did not select anything. So the press stays unconditional and the CLAIM moves into the value:
//
//     edgeHit = Boolean(edgeScreenPoint.onEdge)     // a reading of the product
//     edgeHit = true                               // a fact about the probe
//
// The rule therefore reads the assignment's right-hand side and fails if it is a bare `true`. A
// rule pinned to the old shape would have "found" the wrong thing here and pushed the fix towards
// not pressing at all.

// EVERY `edgeHit = …` in the file, minus the declaration. `let edgeHit = false` is the initialiser
// and matching it would make the gate read `false` — "the field is not the constant true", true of
// the declaration and meaningless about the row. The assignment under test is the one inside the
// press branch, which is the only one after the initialiser.
const assignments = [...src.matchAll(/(?<!let |var )edgeHit = ([^;\n]+);/g)];
check("the source assigns `edgeHit` beyond its initialiser", assignments.length > 0, `found ${assignments.length}`);
const assign = assignments[assignments.length - 1] ?? null;
if (assign) {
  const rhs = assign[1].trim();
  check(
    "`edgeHit` is derived from the hit test rather than the constant `true`",
    /onEdge/.test(rhs) && !/^true$/.test(rhs),
    `assigned \`${rhs}\` — that is a fact about the probe, not a reading of the edge`,
  );
  // The press must still HAPPEN. A probe that skips the press on a point it predicts will miss
  // can never produce the miss branch, and the miss branch is where the evidence for "the curve is
  // obstructed" comes from.
  const window = src.slice(assign.index - 900, assign.index + 200);
  check(
    "the press still runs on a measurable point (the miss branch needs it)",
    /mouse\.down\(\)/.test(window) && /mouse\.up\(\)/.test(window),
  );
  check(
    "the note separates 'did not try' from 'tried and missed'",
    /pressed: edgePressed/.test(window) || /pressed:/.test(src.slice(assign.index, assign.index + 3000)),
  );
}

// ---- 2. both notes carry the evidence beside the number -----------------------------------------

for (const site of noteSites) {
  const step = site.name;
  const body = bodyOf(site.index);
  const carriesHit = /hit:/.test(body);
  if (step === "edge-delete") {
    check("the `edge-delete` note carries a `hit` field", carriesHit);
    check(
      "the `edge-delete` note carries the hit-test evidence beside it",
      /onEdge/.test(body) && /blockedBy/.test(body) && /inViewport/.test(body),
      "a number about the instrument with no reading of the product",
    );
    check(
      "the `edge-delete` note records whether the probe pressed at all",
      /pressed:/.test(body),
      "`hit: false` cannot otherwise be told from a probe that never tried",
    );
  } else {
    // The undo note is about the WRITE, not the pointer: it has no `hit` to carry and gating it on
    // one would be a rule about a field that has never been there. What it must carry is the
    // evidence that the write it reads back arrived -- the same "a row whose gates are measured off
    // a graph nobody committed is unread" property the delete row states.
    check("the `edge-delete-undo` note carries no pointer claim", !carriesHit);
    check(
      "the `edge-delete-undo` note reports the restored count and whether the write settled",
      /restored:/.test(body) && /writeSettled:/.test(body),
      "a `restored` count with no statement about the write that produced it",
    );
  }
}

// ---- 3. the gate itself is honest ---------------------------------------------------------------

check("the gate is GREEN on the unmutated source", fail === 0, `${fail} rule(s) failed`);

console.log(`\n${pass} passed, ${fail} failed`);
if (fail) {
  console.log("\nA filter that silently runs wide is not a focused pass, it is a full pass with a lie in its banner:");
  for (const f of failures) console.log(`  - ${f}`);
}
process.exit(fail === 0 ? 0 : 1);