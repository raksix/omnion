/**
 * The `plugin-palette` walkthrough row has to be able to go RED, and this file is what proves
 * it can.
 *
 * ## Why a unit test reads a QA script
 *
 * The instrument, not the product — the precedent is `keyboard-pass-row.test.ts` and its five
 * siblings. This row earned its file by carrying **the same defect tick 74 found eight lines
 * away in a different row**: it asked the server for `node.node_type`, a field that does not
 * exist.
 *
 * `Node` carries `#[serde(rename = "type")] pub node_type` under `deny_unknown_fields`, so the
 * wire has `type` and nothing else. The row's map therefore produced `undefined` for every node,
 * and the very next line — `!node.node_type.startsWith("trigger.")` — threw a `TypeError` on the
 * FIRST node. That throw happened inside `page.evaluate`, whose `.catch(() => null)` is
 * attached, so it never surfaced: `disabledRead` was `null`, and the note reported
 * `validateStatus: null, saysReEnable: false` — a reading indistinguishable from "the product
 * called a disabled plugin a typo instead of telling the author to re-enable it".
 *
 * **Two rows carrying the same defect is the finding worth keeping.** The tick-74 lesson was
 * written as a principle ("when a reading is structurally impossible, ask whether the gate could
 * ever have gone green") and the very next row in the same file still did it. A principle that
 * lives only in prose does not reach the row written after it; the guards have to be per-row, or
 * they have to be a lint that reads the whole file. **A lesson recorded in a doc is a lesson the
 * next author may skip; a lesson encoded as a red-able assertion is a lesson the next author
 * cannot ship past.**
 *
 * ## What is asserted, and what is deliberately NOT
 *
 * * Every assertion is **structural**: the row must read `type`, must not read `node_type`, and
 *   must make "the probe never reached the server" legible as its own field.
 * * The `null`-legibility assertion is the one that generalises. `saysReEnable: false` is what a
 *   CORRECT product reports for this claim today (there is no plugin store, so the *first* two
 *   claims are honestly false). A row that can only report that one boolean therefore reports a
 *   constant for the criterion — so `pluginProbeReachedServer` is what separates "the product
 *   answered" from "the row never asked".
 * * None of it claims the row passes in a browser. The honest claim is the inverse: a defect in
 *   this row reaches a reading instead of passing silently.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

const RUN_SH = readFileSync(
  new URL("../../../../scripts/qa/run.sh", import.meta.url),
  "utf8",
);

const RUST_GRAPH = readFileSync(
  new URL("../../../../crates/workflows/src/graph.rs", import.meta.url),
  "utf8",
);

/**
 * The row's own source: its banner comment through the note that reports it.
 *
 * The window is a **construct**, and both of its boundaries are derived rather than guessed:
 * the banner `// ----` before the row, and the `});` that closes the note object. A hardcoded line
 * count has killed two of this REQ's instruments outright, and a window that runs past the end
 * of the note is satisfied by the NEXT row's identical field name — which is how a green row
 * once hid a red one.
 */
const rowFor = (step: string): string => {
  const at = WALKTHROUGH.indexOf(`step: "${step}"`);
  assert.notEqual(at, -1, `the walkthrough no longer notes a "${step}" step`);
  const before = WALKTHROUGH.lastIndexOf("// ----", at);
  const head = WALKTHROUGH.slice(before === -1 ? Math.max(0, at - 8000) : before, at);
  const closed = WALKTHROUGH.indexOf("});", at);
  assert.notEqual(closed, -1, "the note must close");
  return head + WALKTHROUGH.slice(at, closed + 3);
};

/** Block comments first, then line comments; string literals deliberately NOT stripped. */
const stripComments = (source: string): string =>
  source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");

const ROW = rowFor("plugin-palette");
/** The same row with its prose removed — every CODE assertion reads this one. */
const ROW_CODE = stripComments(ROW);

const codeOmits = (pattern: RegExp, why: string) => assert.doesNotMatch(ROW_CODE, pattern, why);

test("the wire sends `type`, so the row must not ask for `node_type`", () => {
  // The two halves of one claim, and the reason they are in one test: the first is what makes the
  // second a defect rather than a preference. Asserted over the STRIPPED window, because the
  // sentence describing this very defect lives in the row's own comment — a guard asserted over
  // raw source would stay red forever, which is what happened on the first run of the sibling.
  assert.match(
    RUST_GRAPH,
    /#\[serde\(rename = "type"\)\]\s*\n\s*pub node_type: String/,
    "Node must serialize its type as `type` — if this rename moved, the row's read moves with it",
  );
  codeOmits(
    /\.node_type\b/,
    "the row must not read `node_type`: the server renames it to `type` and denies unknown fields",
  );
});

test("the row renames the node's type in place, on the field the wire has", () => {
  // The rename is the row's whole third claim: a graph carrying the plugin key is pushed through
  // `validate` and the answer has to be the plugin sentence. Reading the wrong field makes the
  // rename a no-op that still produces a *valid* graph, so the finding never fires.
  assert.match(
    ROW_CODE,
    /target\.type = key/,
    "the row must assign the plugin key to `type`, the field `deny_unknown_fields` accepts",
  );
});

test("`null` is reported as its own field, not folded into a verdict", () => {
  // The generalisable half. `saysReEnable: false` is the CORRECT reading for this criterion
  // today, so any row whose only output is that boolean reports a constant — which is precisely
  // how the `TypeError` above survived. The probe's reachability has to be its own number.
  assert.match(
    ROW_CODE,
    /pluginProbeReachedServer: disabledRead !== null/,
    "the note must report whether the probe reached the server, so a `null` is not read as a verdict",
  );
  assert.match(
    ROW_CODE,
    /pluginProbeSkipped/,
    "a probe that named no target must say so instead of leaving `null` to be interpreted",
  );
});

test("the two sentences stay distinguishable, and the criterion's three claims are separate", () => {
  // The criterion is three claims and only the middle one needed a store. Collapsing them into
  // one "passed" flag is how a row reports a palette that cannot draw anything as a palette that
  // was correct to draw nothing.
  for (const field of ["enabledNow", "badgeRendered", "tooltipNamesProvider", "sentinelsStayApart"]) {
    assert.match(
      ROW_CODE,
      new RegExp(`\\b${field}\\b`),
      `the note must report \`${field}\` on its own — the criterion counts claims, not passes`,
    );
  }
});

test("a disabled plugin must not read as a typo, and the two lookups must be DISTINCT", () => {
  // **This guard was leaky and its mutation caught it.** M4 rewrites
  // `saysTypo: Boolean(typoSentence)` to `Boolean(pluginSentence)` — one sentence searched under
  // the other's name — and the suite stayed green, because the assertion before this one only
  // checked that the FIELDS `saysReEnable`/`saysTypo` exist. A presence check cannot tell two
  // variables that were both derived from the same lookup apart, and "both fields are there" was
  // never the claim: the claim is that the product's two sentences are searched for separately,
  // so that the absence of the typo sentence is evidence.
  //
  // Hence the assertions are on the BINDINGS, not the names: each sentence must come from its own
  // regex match over the findings. That is what a mutation has to defeat to stay green, which is
  // what it just failed to do.
  assert.match(
    ROW_CODE,
    /pluginSentence = unknownFindings\.find\(\(finding\) => \/re-enable\/i\.test\(finding\.message\)\)/,
    "the plugin sentence must come from its own search for \"re-enable\"",
  );
  assert.match(
    ROW_CODE,
    /typoSentence = unknownFindings\.find\(\(finding\) => \/not a node type the platform knows\/i\.test\(finding\.message\)\)/,
    "the typo sentence must come from its own search, or its absence proves nothing",
  );
  assert.match(
    ROW_CODE,
    /sentinelsStayApart: Boolean\(pluginSentence\) && !typoSentence/,
    "the contract is the plugin sentence present AND the typo sentence absent",
  );
  // **M4 is what forced these two.** It rewrote `saysTypo: Boolean(typoSentence)` to
  // `Boolean(pluginSentence)` — one sentence reported under the other's name — and the suite was
  // green, because `sentinelsStayApart` (the thing the criterion is about) was untouched and the
  // field-presence check cannot tell two variables apart. The mutation was not a strawman: the
  // NOTE is what a reader reads, and a note whose `saysTypo` is really `saysReEnable` reports
  // "the product called it a typo" on a run that proved the opposite. A diagnostic that lies is
  // worse than a diagnostic that is missing, so both report bindings are asserted here.
  assert.match(
    ROW_CODE,
    /saysReEnable: Boolean\(pluginSentence\)/,
    "`saysReEnable` must report the plugin sentence, not some other lookup",
  );
  assert.match(
    ROW_CODE,
    /saysTypo: Boolean\(typoSentence\)/,
    "`saysTypo` must report the typo sentence — a reader reads this field as a product verdict",
  );
});

test("run.sh lets the artifact root move off a full disk", () => {
  // **The third defect this tick, and the same shape as the second: a hardcoded path where a
  // knob belongs.** `OUT` was `$ROOT/qa-artifacts/$TS`, so a pass on a box at 99% wrote its
  // ~100MB of screenshots into the disk that was already full and recorded `ENOSPC` on every one
  // — the run then *looked* like it had measured the panel while producing nothing a reader
  // could open. That is the failure mode of every fix in this REQ: a measurement that silently
  // does not happen, reported as one that did.
  //
  // Asserted on the override AND on its default, because an override that replaced the default
  // outright would satisfy the first and lose the convention.
  assert.match(
    RUN_SH,
    /OUT="\$\{QA_OUT_ROOT:-\$ROOT\/qa-artifacts\}\/\$TS"/,
    "the artifact root must be overridable while defaulting to the worktree",
  );
  // **The stronger claim, and the one the first draft got wrong.** I first asserted that
  // run.sh surfaces "screenshot failed" — it does not, the walkthrough does — so that assertion
  // was checking a string in the wrong file and would have been either dead or trivially true
  // once moved. What actually matters is the failure being RECORDED rather than swallowed: the
  // walkthrough catches a screenshot error and logs it, and `shots` only ever receives entries
  // that were written. A run whose screenshots all failed therefore reports zero shots, which is
  // a number a reader can see, instead of a pass that looks complete.
  assert.match(
    WALKTHROUGH,
    /catch \(err\) \{\s*\n\s*log\(`screenshot failed for \$\{name\}: \$\{err\.message\}`\)/,
    "a screenshot that cannot be written must be logged, so a run with no shots is legible",
  );
});

test("run.sh accepts `--only` on the command line, or a focused pass silently runs wide", () => {
  // **The second defect this tick, and it is in the same box.** `run.sh` read `${QA_ONLY:-}` and
  // never `$1`, so `bash scripts/qa/run.sh --only=workflow-builder` ran the ENTIRE route list:
  // not an error, not a warning, not a line in the log. The flag's only job is the one job it
  // silently did not do.
  //
  // Both spellings are asserted because a filter reachable only one way is a filter that gets
  // missed — that is the whole defect. `--only VALUE` needs the cursor this script gives it, and
  // `--only=VALUE` is the spelling a human types under time pressure.
  assert.match(
    RUN_SH,
    /--only=\*\) QA_ONLY_FILTER=/,
    "run.sh must accept `--only=VALUE` from the command line (the literal `=` needs no escape)",
  );
  // Asserting the CURSOR (`_prev` is set and read) rather than the exact comparison text: the
  // shape of that `[ … ]` test is a formatting choice, while "the loop remembers the previous
  // argument" is the behaviour `--only VALUE` depends on. A guard that pins the whole line
  // breaks on a reformat and passes on a cursor that is written but never read.
  assert.match(
    RUN_SH,
    /_prev=""/,
    "run.sh must initialise the previous-argument cursor for the `--only VALUE` spelling",
  );
  assert.match(
    RUN_SH,
    /"\$_prev" = "--only"/,
    "run.sh must read the cursor back, so the argument after a bare `--only` becomes the filter",
  );
});

test("the pass banner reports the filter it actually applied", () => {
  // A report that misstates its own scope is worse than no report: the banner is the line a
  // reader trusts to know what was covered. With the flag arriving on the command line the env
  // var is empty, so a banner reading `${QA_ONLY}` announced a full pass over a narrow one.
  assert.match(
    RUN_SH,
    /step "browser walkthrough\$\{QA_ONLY_FILTER:\+/,
    "the walkthrough banner must read the resolved filter, not the env var the flag may bypass",
  );
  assert.doesNotMatch(
    RUN_SH,
    /step "browser walkthrough\$\{QA_ONLY:\+/,
    "the banner must not read `QA_ONLY` — it is empty when the flag came from the command line",
  );
});