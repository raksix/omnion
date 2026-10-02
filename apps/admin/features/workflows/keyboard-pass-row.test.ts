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

// ---------------------------------------------------------------------------------------------
// The run read (tick 89). Four defects, one fetch, and a header above that describes the CLASS of
// bug this file exists to catch — so the class shipped again anyway, three tests below the row it
// was written about.
// ---------------------------------------------------------------------------------------------

test("the row asks the server for a route this server actually mounts", () => {
  // **The first defect: `/workflows/{id}/runs` was never a route.** The run list is
  // `/workflows/{id}/executions`; `runs` exists only under `/media/scan` and
  // `/media/retention`. The fetch 404s, `if (!response.ok) return null` swallows the 404, and
  // `runsAfterKey` is `null` on every run — indistinguishable, in a report, from a keyboard
  // that starts nothing.
  //
  // This assertion is over the Rust router, not over a list written here: a hand-kept
  // allowlist of "the paths that are real" is the same defect one layer out, and it would drift
  // the first time a sibling mounted a route. Every path this row fetches has to be one the
  // router declares.
  const MOUNTED = readFileSync(new URL("../../../../apps/api/src/routes/mod.rs", import.meta.url), "utf8");
  for (const segment of ROW_CODE.match(/\/api\/v1\/workflows\/\$\{id\}\/([a-z-]+)/g) ?? []) {
    const tail = segment.replace("/api/v1/workflows/${id}/", "");
    assert.match(
      MOUNTED,
      new RegExp(`\\.route\\(\\s*"/workflows/\\{id\\}/${tail}"`),
      `the row fetches /workflows/{id}/${tail}, which the router does not mount — ` +
        `a 404 here reads exactly like a keyboard that starts nothing`,
    );
  }
  // …and it asserts the row actually fetches one, because a sweep over an empty list is a
  // sweep that cannot fail. This is the control that bites (tick 87's rule).
  assert.match(
    ROW_CODE,
    /\/api\/v1\/workflows\/\$\{id\}\/[a-z-]+/,
    "the row must fetch at least one workflow route, or the sweep above proves nothing",
  );
});

test("the row reads the payload field the server sends", () => {
  // **The second defect: `body.runs` is not a field.** `ExecutionListResponse` is
  // `{ workflow_id, executions }` — the struct renames the *column* (`trigger_kind`) to a
  // *wire* key (`trigger`) in `ExecutionSummary`, and this row read the column name. Both
  // halves are asserted against the Rust type, because "the row must not say `.runs`" alone
  // would pass on a server renamed to `/runs/foo`.
  const ROUTES = readFileSync(
    new URL("../../../../apps/api/src/routes/workflows.rs", import.meta.url),
    "utf8",
  );
  assert.match(
    ROUTES,
    /pub struct ExecutionListResponse\s*\{[\s\S]*?pub executions: Vec<ExecutionSummary>/,
    "the run list's payload key is `executions` — if this field moved, the row's read moves with it",
  );
  assert.match(
    ROUTES,
    /pub trigger: String/,
    "`ExecutionSummary` sends the run's origin as `trigger`; `trigger_kind` is the STORED column",
  );
  codeOmits(
    /\?\.\s*runs\b/,
    "the row must not read `body.runs`: the payload is `executions`",
  );
  codeOmits(
    /trigger_kind/,
    "the row must not read `trigger_kind`: `ExecutionSummary` renames the column to `trigger` on the wire",
  );
  assert.match(ROW_CODE, /\?\.executions/, "the row must read the payload the server actually sends");
});

test("a run count is reported beside its baseline, never alone", () => {
  // **The third defect, and the one the other two were hiding.** The builder pass above pressed
  // `Run from here` on this same rule moments before the keyboard pass presses `r`, so the
  // rule already has a run. A count of "1 run exists" is satisfied by the row above's press, and
  // the old row reported exactly that as `startedFromKey` — a green answer about a key that was
  // never pressed. The count cannot distinguish "the keyboard started this" from "something
  // else did, recently", so the before-count is the evidence and it travels with the after.
  assert.match(
    ROW_CODE,
    /runCountBeforeKey/,
    "the run count needs a baseline taken before the press",
  );
  assert.match(
    ROW_CODE,
    /Math\.max\(0,\s*runAfterKeyList\.length - runCountBeforeKey\)/,
    "the claim must be a DIFFERENCE against the baseline, and clamped at zero",
  );
  // …and the difference has to reach the NOTE, or the baseline is collected and never read.
  //
  // **The first draft of this assertion was satisfied by the DECLARATION.** It searched the
  // whole row window for the field name, and the window starts at the banner — which includes
  // `const runCountBeforeKey = …`, above `step:`. Deleting the field from the note object left
  // the name in the window and the gate green (M7 survived). A field name inside a note is
  // only evidence if it is inside the note's own braces, so the window here is the object
  // literal: from `step:` to the `});` that closes it, with nothing before `step:`.
  //
  // The window is found by balance, not by a hardcoded line count — two of this REQ's
  // instruments died on the latter (tick 57's lesson) — and the comment above `rowFor` is the
  // other half of the same trap: anchoring on the NEXT row's banner pulls in a different note's
  // fields.
  const noteStart = ROW.indexOf('step: "keyboard-pass"');
  const noteEnd = ROW.indexOf("});", noteStart);
  assert.notEqual(noteEnd, -1, "the keyboard-pass note must close");
  // …and the window is read with its prose removed, because the note's OWN comment explains the
  // three fields by name: deleting `runCountBeforeKey,` from the object left it in the comment
  // directly above and the gate green again (M7 survived twice, for two different reasons).
  // This is the same rule the first test in this file states — a guard that trips on the
  // sentence describing the defect cannot police the defect — and it took a second draft to
  // apply it here, which is worth the record.
  const note = stripComments(ROW.slice(noteStart, noteEnd + 3));
  for (const field of ["runCountBeforeKey", "runsAfterKey", "runsAddedByKey"]) {
    assert.match(
      note,
      new RegExp(`\\b${field}\\b`),
      `the note must carry \`${field}\` — a baseline collected in the row body and absent from ` +
        `the note is a number no reader sees`,
    );
  }
  // A count beside a baseline is still a count; the claim is the boolean the criterion wants.
  assert.match(
    note,
    /runStartedFromKey:\s*runsAddedByKey > 0/,
    "the note must name the claim the criterion states — a run the KEY started",
  );
});

test("an unreadable endpoint is not reported as a key that started nothing", () => {
  // The `null` collapse is what let the first defect survive: a 404, a dead stack and a 401 all
  // arrive as `null`, and so does "the endpoint answered and the list was empty". Those are four
  // different facts and the row reported one. `runsAddedByKey` stays `null` unless BOTH counts
  // are numbers, so "the endpoint did not answer" can never read as a zero.
  assert.match(
    ROW_CODE,
    /runsAddedByKey > 0 \? runAfterKeyList\[0\] : null/,
    "the run whose origin is reported must be one the key produced, not index 0 of a list",
  );
  // **The guard has to be about REACHABILITY, not about the presence of a substring.** The first
  // draft asserted `runAfterKeyList !== null && runCountBeforeKey !== null` appears in the row,
  // which `&& false` satisfies without changing a single value the note reports (M4 survived):
  // the condition is still *written* and no longer *used*. This is tick 85's rule — the press was
  // left unguarded there for the same reason — and it is the "a claim about a use, answered by a
  // claim about a mention" defect that this file's own header was written to prevent.
  //
  // The condition is therefore extracted and checked for a short-circuit on a constant, and the
  // assignment is checked to actually be that ternary. A strip of the whole guard is caught by
  // the second half; a `&& false` by the first.
  const diffStart = ROW_CODE.indexOf("const runsAddedByKey");
  assert.notEqual(diffStart, -1, "the difference must be assigned somewhere in the row");
  const diffWindow = ROW_CODE.slice(diffStart, diffStart + 320);
  assert.match(
    diffWindow,
    /^const runsAddedByKey\s*=\s*runAfterKeyList !== null && runCountBeforeKey !== null\s*\n?\s*\?/,
    "`runsAddedByKey` must be assigned the ternary guarded by BOTH counts being real numbers",
  );
  assert.doesNotMatch(
    diffWindow.slice(0, diffWindow.indexOf("?")),
    /&&\s*(false|true|0|undefined|null)\b/,
    "the difference's guard short-circuits on a constant — the condition is written and unused",
  );
});
