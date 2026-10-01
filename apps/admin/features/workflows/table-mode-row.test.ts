/**
 * The `workflow-table` row has to be able to go RED, and this file is what proves it can.
 *
 * ## Why this row is different from the ones next to it
 *
 * `table-mode.test.ts` tests the table's RULES — `buildTable`, `diffTableEdits`, `toGraph` — and
 * every one of those can be correct while the row that reports them renders nothing. A unit test
 * of `toGraph` proves the pure function commits the right graph; it cannot prove the browser
 * asked, or that the canvas the criterion names was ever opened.
 *
 * That gap is not hypothetical, because this is the **only** row in the builder pass with no
 * instrument test of its own. `step-trace-row`, `run-from-here-row`, `undo-selection-edge-row`,
 * `reload-rebase-row` all have one, and each was written because its row measured the wrong
 * thing. The pattern in every one of them: **the criterion names a surface, and the row read a
 * different one.**
 *
 * ## The defect this file was written for
 *
 * The criterion is "Table mode renders the same definition, edits parameters, and stays
 * consistent with the canvas after a save in either mode." The last clause is a claim about
 * the **canvas**, and the row asserts it with a `fetch` of the server:
 *
 * ```js
 * const builderSeesTableEdit = await page.evaluate(async (id) => {
 *   const current = await (await fetch(`/api/v1/workflows/${id}/graph`, …)).json();
 *   return Object.values(current.graph.nodes ?? {}).some((n) =>
 *     Object.values(n.params ?? {}).includes("qa.table.edited"));
 * }, workflowId);
 * ```
 *
 * The field is named `builderSeesTableEdit` and the navigation above it goes to the builder, so
 * the note reads as a claim about the builder's screen. It is a claim about Postgres. A canvas
 * that rendered no node at all, one that failed to mount the inspector, or one whose inspector
 * never received the graph would all report `true` — and the `goto` immediately above it is
 * exactly the navigation that makes an author believe the canvas was consulted.
 *
 * This is the tick-57 defect verbatim, one block down: there, `step.output` was read off the
 * wire while the criterion was about the panel. Here, the graph is read off the wire while the
 * criterion is about the canvas. **The product already publishes the markers** — the inspector
 * writes `data-inspector` for the node and `data-inspector-field` for each parameter input — so
 * the read can be made honest cheaply.
 *
 * ## Why every assertion is about a CONSTRUCT, never a name
 *
 * The same discipline as `undo-selection-edge-row.test.ts`, and for the same reason: a name
 * that appears twice satisfies an assertion made about one of the two. `data-inspector` is a
 * PREFIX of `data-inspector-field`, so an assertion made with `includes("data-inspector")`
 * is satisfied by a row that reads only the fields. The assertions below name the selector that
 * does the work and the window that contains it.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/** The whole table-mode block, from the create to the last note it reports. */
const BLOCK = (() => {
  // The enclosing function, not a guess at it: the name `runTableMode` was an invention of the
  // first draft and `indexOf` returned -1, which surfaced as a failing `assert.notEqual` with
  // `actual: -1, expected: -1` — a message that reads like the row is missing when the
  // ANCHOR was. Every window in this file is anchored on a real construct for that reason.
  const start = WALKTHROUGH.indexOf("async function runWorkflowTableDepth(page, report) {");
  assert.notEqual(start, -1, "the table-mode block must exist in the walkthrough");
  // The window runs to the END of the block's last note, not to the first token of it. Ending
  // at `step: "unfinished-run-refused"` cut the two closing notes out of the window entirely,
  // so `BLOCK.indexOf` for them returned -1 and the tests reported "the note is missing" when
  // the WINDOW was short — the same one-sided-window defect this file exists to prevent, made
  // by the file itself. The end anchor is the following block, not a note inside this one.
  const end = WALKTHROUGH.indexOf("async function ", start + 64);
  assert.notEqual(end, -1, "the block must be followed by another function");
  return WALKTHROUGH.slice(start, end);
})();

/**
 * The reverse-direction read, in its OWN window.
 *
 * The row asserts the same clause in two directions, and the first direction (a canvas rename
 * reaching the table) is a real DOM read off `[data-table-label]`. This window is the other
 * one. A window covering the whole block would be satisfied by the *first* read — the same
 * one-sided measurement tick 56 spent itself on, one row over, and the reason every window in
 * this file is per-note rather than per-block.
 */
const REVERSE_READ = (() => {
  // Anchored on `canvasRead`, the read's own name. The first draft anchored on
  // `builderSeesTableEdit`, and the fix renamed the binding — which broke the test with
  // `actual: -1` and a message reading "the read must exist" when the ANCHOR had gone stale.
  // That is the third time in this file that an assertion reported on its own anchor (see the
  // `BLOCK` and `end` comments), which is why the rule is now written down: a window in a
  // harness test is a CONTRACT with the row, and the fix that satisfies the contract is
  // allowed to rename the row's variables.
  // The window OPENS at the navigation, not at the read. The waits that make the read safe sit
  // BETWEEN the `goto` and the `evaluate`, so a window starting at the read excludes them and
  // the assertion "the read must wait for the inspector" was red against a row that does — the
  // tenth instance in this REQ of the one class this file keeps rediscovering: a window that
  // does not contain the construct its assertion is about.
  // The `goto` is the LAST one before the read, so the search starts from the read's own
  // position and walks BACK to the navigation that opened this direction. A forward search
  // would find the block's FIRST `goto` (the one to the builder to read the table-mode link)
  // and produce a window that spans the whole forward-direction section.
  const readAt = BLOCK.indexOf("const canvasRead = await page");
  assert.notEqual(readAt, -1, "the reverse-direction read must exist");
  const start = BLOCK.lastIndexOf(
    "await page.goto(`${admin}/workflows/${workflowId}/builder`",
    readAt,
  );
  assert.notEqual(start, -1, "the row must navigate to the builder before reading it");
  assert.ok(start < readAt, "the navigation must precede the read");
  const end = BLOCK.indexOf('note({ step: "table-save-survives"', start);
  assert.notEqual(end, -1, "the reverse read must be followed by its note");
  assert.ok(end > start, "the window must not end before it begins");
  return BLOCK.slice(start, end);
})();

test("the reverse direction is read off the CANVAS, not off the server", () => {
  // The whole point of this file. The criterion's clause is about the canvas; a `fetch` answers
  // a different question and the `goto` above it makes the note look like it consulted one.
  assert.ok(
    /data-inspector/.test(REVERSE_READ),
    "the builder must be read through the markers it renders (data-inspector), not through a fetch of the graph",
  );
  assert.ok(
    !/fetch\(`\/api\/v1\/workflows\/\$\{id\}\/graph`/.test(REVERSE_READ),
    "a fetch of the graph is a claim about the server; the criterion's clause is about the canvas",
  );
});

test("the canvas read compares against the VALUE it was handed, not merely that a value exists", () => {
  // `Object.values(n.params).length > 0` satisfies "a value changed" and tells you nothing
  // about whether the TABLE's value survived. The assertion is on the COMPARISON — `f.value ===
  // value` — and not on the presence of the literal `qa.table.edited`, because the literal is
  // also the `evaluate` argument and is in the window either way.
  //
  // M2 is the proof: replacing the comparison with "any non-empty field" left this suite GREEN
  // in its first draft, which asserted only that the string appears somewhere near the read.
  // A value is not in the report because it was typed into it.
  assert.ok(
    /found: fields\.some\(\(f\) => f\.value === value\)/.test(REVERSE_READ),
    "the read must compare each field against the value it was handed",
  );
  assert.ok(
    /data-inspector-field/.test(REVERSE_READ),
    "and it must come off the parameter inputs themselves, which is where the value is rendered",
  );
});

test("the panel read is scoped to the NODE, not to the page", () => {
  // `data-inspector` is a PREFIX of `data-inspector-field`. So an assertion made with the bare
  // name — which is what the first draft of the test above used — is satisfied by a row that
  // reads only the field markers and never resolves a panel at all. M3 is the proof: setting
  // `panel` to `document` left the suite green.
  //
  // The distinguishing construct is the node-scoped SELECTOR, and the attribute name is not
  // enough: the fix here must interpolate the node id, because an inspector that rendered one
  // panel for whatever was last selected answers the same question for every node.
  assert.ok(
    /document\.querySelector\(`\[data-inspector="\$\{nodeId\}"\]`\)/.test(REVERSE_READ),
    "the read must resolve the panel FOR THE NODE, or every node answers with the last selection",
  );
  assert.ok(
    !/const panel = document;/.test(REVERSE_READ),
    "and it must not fall back to the whole document, which makes nodeId meaningless",
  );
});

test("the forward direction is read off the table DOM, not off the graph", () => {
  // The other half of "in either mode", and the one a table holding its own private copy of the
  // graph gets wrong. `[data-table-label]` is the table's own marker; reading the graph instead
  // would pass against a table that re-renders from what it loaded and drops the author's
  // canvas edit from the list entirely.
  const start = BLOCK.indexOf('step: "canvas-save-visible"');
  assert.notEqual(start, -1, "the forward-direction note must exist");
  const end = BLOCK.indexOf('step: "table-save-survives"', start);
  assert.notEqual(end, -1, "the forward note must come before the reverse one");
  const window = BLOCK.slice(BLOCK.lastIndexOf("labelsAfterCanvasSave = await page", start), end);
  assert.ok(
    /data-table-label/.test(window),
    "the canvas rename must be read off the table's own label inputs",
  );
  assert.ok(
    /seesCanvasRename: labelsAfterCanvasSave\.includes\("Renamed on the canvas"\)/.test(window),
    "and the assertion must be the literal the canvas write used, or a renamed node passes for any rename",
  );
});

test("the create note reports the REFUSAL, so a 422 cannot read as 'nothing rendered'", () => {
  // This block's first note is the one every later row sits downstream of, and for three ticks
  // the create came back 422 (`StepDefinition` is `deny_unknown_fields`, so it names the
  // missing field) — which returned the block early and left every row below it unmeasured
  // rather than red. A note that recorded only a status made that indistinguishable from a
  // table that renders nothing. The refusal MESSAGE is what turns it into a next action.
  const start = BLOCK.indexOf('step: "create"');
  assert.notEqual(start, -1, "the create note must exist");
  const end = BLOCK.indexOf('step: "builder-link"', start);
  assert.notEqual(start !== -1 && end > start, "the create note must come before the builder link note");
  const window = BLOCK.slice(start, end);
  assert.ok(
    /refusal: created\.status >= 400 \? \(created\.body\?\.error\?\.message \?\? ""\)\.slice\(0, 200\) : null/.test(
      window,
    ),
    "the create note must carry the server's refusal message, or a 422 reads as an empty table",
  );
  assert.ok(
    /if \(!workflowId\)/.test(window),
    "and the block must bail on a refused create, so the rows below cannot report on a rule that does not exist",
  );
});

test("the edit asserts against the SERVER'S copy, and says so in the note", () => {
  // The one place a `fetch` is right: the field is uncontrolled, so a table that never read the
  // input back looks correct until the author reloads. But the note field is what makes it
  // checkable, and `wroteToServer` is the gate the criterion is written against.
  const start = BLOCK.indexOf('step: "edit-saves"');
  assert.notEqual(start, -1, "the edit note must exist");
  const window = BLOCK.slice(start, start + 1400);
  assert.ok(
    /wroteToServer:/.test(window),
    "the edit must be asserted against the server's copy after a save",
  );
  assert.ok(
    /saveDisabledWhenClean: saveDisabledBefore/.test(window),
    "an unedited draft must be reported as not committable, or a write advances graph_version for nothing",
  );
  assert.ok(
    /saveEnabledWhenDirty: saveEnabledDirty === false/.test(window),
    "and the dirty half beside it, since the two are one claim about the Save control",
  );
});

test("the run refusal is read off the screen's SENTENCE, and a refusal leaves no run behind", () => {
  // 400 and not 403/404/409: the caller is allowed to try, this rule is not ready. The message
  // has to be a sentence about THIS graph, and a refusal that created an execution row would
  // make a rule that never fired look like it did.
  const start = BLOCK.indexOf('step: "unfinished-run-refused"');
  assert.notEqual(start, -1, "the run-refusal note must exist");
  const window = BLOCK.slice(start, start + 900);
  assert.ok(
    /refused: runResponse\.status === 400/.test(window),
    "the run must be refused with 400, the status that means 'not ready' rather than 'not allowed'",
  );
  assert.ok(
    /messageNamesTheGraph: Boolean\(\(runResponse\.message \?\? ""\)\.trim\(\)\)/.test(window),
    "the refusal must name the graph rather than being a generic error",
  );
  assert.ok(
    /executionsAfter:/.test(window),
    "and a refused run must leave no execution behind",
  );
});

test("the problems panel is read for the FINDING, not for 'no problems'", () => {
  // "No problems" over a graph the server will not run is the exact failure the finding list
  // was made for, and `saysNone` is the negative control — a panel that always rendered the
  // empty state would pass the criterion's wording. Both halves are asserted because the
  // finding and its absence are one claim.
  const start = BLOCK.indexOf('step: "unfinished-problems-panel"');
  assert.notEqual(start, -1, "the problems-panel note must exist");
  const window = BLOCK.slice(start, start + 900);
  assert.ok(
    /notClaimingClean: !problemsAfterUnfinishedSave\.saysNone/.test(window),
    "the panel must not claim a graph with a missing parameter is clean",
  );
  assert.ok(
    /namesTheMissingParameter: problemsAfterUnfinishedSave\.listed\.includes\("missing_parameter"\)/.test(
      window,
    ),
    "and it must name the parameter that is missing",
  );
});

test("the row navigates to the builder BEFORE reading it, in the same window", () => {
  // `waitForTimeout(1500)` after a `goto` is the tick-57 race: the read can land before the
  // canvas has mounted, and every marker assertion would then be green against a page that
  // had not drawn yet. The `goto` and the read must be adjacent so the ordering is the claim,
  // and the read waits for the marker it needs rather than a fixed delay.
  const goto = BLOCK.indexOf("await page.goto(`${admin}/workflows/${workflowId}/builder`", BLOCK.indexOf('step: "canvas-save-visible"'));
  assert.notEqual(goto, -1, "the row must open the builder before reading it");
  const readAt = BLOCK.indexOf("const canvasRead = await page");
  assert.notEqual(readAt, -1, "the read must exist");
  assert.ok(
    goto < readAt,
    "the navigation must precede the read, or the read is of whatever was on screen before",
  );
  assert.ok(
    /waitForSelector\("\[data-inspector\]"/.test(REVERSE_READ),
    "the read must wait for the inspector to mount rather than a fixed delay",
  );
});
