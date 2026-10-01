/**
 * The `keyboard-pass` walkthrough row has to be able to go RED, and this file is what proves
 * it can.
 *
 * ## Why a unit test reads a QA script
 *
 * The instrument, not the product — the precedent is `table-mode-row.test.ts` and
 * `reload-rebase-row.test.ts`. This row earned the file: **its parameter read asked the server
 * for `node.node_type`, a field that does not exist.** `Node` carries `#[serde(rename = "type")]`
 * on `node_type`, and the struct is `deny_unknown_fields`, so the wire has `type` and nothing
 * else. The comparison was `undefined === "wait"` — false on every run, forever — and the row's
 * `paramReadBack` was structurally `null` with `paramWrote` reporting `false`.
 *
 * That is the shape this file exists for, and it is worse than a row that measured the wrong
 * surface: **a gate that cannot go green and a gate that cannot go red read identically in a
 * report.** Every tick that cited "the keyboard row shows `paramWrote: false`" was citing a
 * constant, and the correct reading of it is "this row never had a subject", not "the keyboard
 * cannot edit a parameter". Nothing about the number would have signalled the defect.
 *
 * The second half is the generalisable rule. Finding "the node the keystrokes landed in" by its
 * *type* is a lookup that resolves for whichever node happens to match, so on a graph holding a
 * `wait` node from an earlier block it reads a parameter **the keyboard never touched** and calls
 * it `paramWrote`. The row is about a path, and a path measured at the wrong node is a green
 * answer about a product the pass never exercised. Hence `paramSubjectKnown` in the note, and
 * `paramWrote` requiring it.
 *
 * ## What is asserted, and what is deliberately NOT
 *
 * * Every assertion is **structural**: the row asks the server for `type`, resolves the subject
 *   from the focused field's own `id`, and refuses to report a write without a subject.
 * * `type` is not asserted to be the only spelling — it is asserted to be the one the row READS,
 *   so a future rename cannot leave a constant behind silently. The Rust guard in
 *   `node-status.test.ts` carries the other half (that the wire key is `type`).
 * * None of it claims the row passes in a browser. The honest claim is the inverse: a defect in
 *   the inspector reaches a reading instead of passing silently.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

const RUST_GRAPH = readFileSync(
  new URL("../../../../crates/workflows/src/graph.rs", import.meta.url),
  "utf8",
);

/** The row's own source: its banner comment through the note that reports it. */
const rowFor = (step: string): string => {
  const at = WALKTHROUGH.indexOf(`step: "${step}"`);
  assert.notEqual(at, -1, `the walkthrough no longer notes a "${step}" step`);
  // **The window must CONTAIN the whole note, and that is two boundaries, not one.** The
  // obvious window — "from the row's opening line to its `note`" — ends on `step:`, the FIRST
  // key of the note call, and so excludes every field the note REPORTS. Anchoring on the next
  // row's banner instead fixes that and creates the mirror failure: it then runs on to the
  // following row's code, where a `paramWrote` or a `paramSubjectKnown` belonging to a DIFFERENT
  // note would satisfy these assertions. Five of this file's tests were written against one
  // boundary or the other, and each was wrong for the opposite reason.
  //
  // The window is therefore the banner BEFORE the row to the end of the note's own object, and
  // the end is found by balance rather than by a guess at the formatting: `step: "<name>"` plus
  // a scan for the `});` that closes it. Two of this REQ's instruments died on a hardcoded line
  // count, so the boundary here is derived, and the control below re-proves it.
  const before = WALKTHROUGH.lastIndexOf("// ----", at);
  const head = WALKTHROUGH.slice(before === -1 ? Math.max(0, at - 8000) : before, at);
  // The note ends at the first `});` after its `step:` key — this row's note is the only object
  // literal opened between the banner and the closing call.
  const closed = WALKTHROUGH.indexOf("});", at);
  assert.notEqual(closed, -1, "the note must close");
  return head + WALKTHROUGH.slice(at, closed + 3);
};

/** Block comments first, then line comments; string literals deliberately NOT stripped. */
const stripComments = (source: string): string =>
  source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");

const ROW = rowFor("keyboard-pass");
/** The same row with its prose removed — every CODE assertion reads this one. */
const ROW_CODE = stripComments(ROW);

/**
 * Assert a construct is ABSENT from the row's code, and say so in the failure.
 *
 * `assert.ok(!/x/.test(...))` reports "the expression evaluated to false" and nothing about what
 * was wanted, which is how a red in this file once read as "the row is missing" when the row
 * was present and the *window* was wrong. `assert.doesNotMatch` names the pattern.
 */
const codeOmits = (pattern: RegExp, why: string) => assert.doesNotMatch(ROW_CODE, pattern, why);

test("the wire sends `type`, so the row must not ask for `node_type`", () => {
  // The two halves of one claim, and the reason they are in one test: the first half is what
  // makes the second a defect rather than a preference. A guard that only forbids
  // `node.node_type` would pass on a server that renamed the field to something else, and a
  // guard that only pins `type` would pass on a row reading `node_type`.
  assert.match(
    RUST_GRAPH,
    /#\[serde\(rename = "type"\)\]\s*\n\s*pub node_type: String/,
    "Node must serialize its type as `type` — if this rename moved, the row's read moves with it",
  );
  // Read the STRIPPED window: this file's own documentation records that a guard which trips on
  // the sentence describing the defect cannot police the defect, and the sentence describing
  // THIS defect is in this row's comment. Asserted over raw source, the fix's own prose would
  // keep it red forever — which is what happened on the first run.
  codeOmits(
    /\.node_type\b/,
    "the row must not read `node_type`: the server renames it to `type` and denies unknown fields",
  );
});

test("the row resolves the subject from the element's own id, not from the node's type", () => {
  // A lookup by type resolves for SOME node on any graph that happens to hold one, which is how
  // a row reports a parameter it never typed. The subject has to come from the field the
  // keystrokes land in, and `NodeInspector` renders it as `id={param-${node.id}-${key}}`.
  assert.match(
    ROW_CODE,
    /\^param-/,
    "the row must parse the node id out of the focused field's `param-<nodeId>-<key>` id",
  );
  assert.match(
    ROW_CODE,
    /paramSubjectKnown/,
    "the note must report whether the subject was resolved, so a fallback read is legible",
  );
});

test("a parameter write is not reported without a subject", () => {
  // The fallback (`find(n => n.type === "wait")`) stays in the row on purpose: when the subject
  // is unresolvable the read is better than nothing. What must not happen is that read being
  // reported AS the keyboard's write, which is the defect this row shipped with.
  assert.match(
    ROW_CODE,
    /paramWrote:\s*paramReadFrom !== null && \(/,
    "`paramWrote` must require a resolved subject; otherwise it reports another node's value",
  );
});

test("the row adds two nodes and reads the edge and the parameter from the server", () => {
  // The criterion is a sequence of five verbs. A canvas that holds an uncommitted edge and a
  // field that renders an uncommitted value would satisfy every DOM assertion in this row, so
  // the two readings that matter are the server's copies.
  assert.match(
    ROW_CODE,
    /await page\.keyboard\.press\("Control\+p"\)/,
    "the row must reach the palette by keyboard (⌘P), not by pointer",
  );
  assert.match(
    ROW_CODE,
    /readGraphAgain\(\)/,
    "the edge and the parameter must be read back off the server",
  );
  // Two verbs, two assertions: a single `assert.match` over a regex carrying both would pass on
  // a row that dropped EITHER one, which is the count-versus-identity mistake this file's
  // neighbours keep documenting.
  for (const verb of ["v", "r"]) {
    assert.match(
      ROW_CODE,
      new RegExp(`page\\.keyboard\\.press\\("${verb}"\\)`),
      `validate and run are the last two verbs and are single keys (missing "${verb}")`,
    );
  }
});

test("the row drives no pointer event, because the criterion is the path", () => {
  // One `locator.click()` anywhere in this block makes the whole note untrustworthy: a path
  // that quietly fell back to a mouse passes every assertion above. The rule is asserted over
  // the row's own source, so it cannot be broken by a well-meaning edit that reaches for the
  // pointer to "make the pass more reliable".
  for (const forbidden of [/page\.mouse\./, /\.dragTo\(/, /locator\([^)]*\)\s*\.first\(\)\s*\.click\(/]) {
    codeOmits(
      forbidden,
      `the keyboard row must not use ${forbidden} — the criterion is that the pointer is untouched`,
    );
  }
});
