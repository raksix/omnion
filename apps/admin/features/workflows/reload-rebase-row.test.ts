/**
 * The `reload-rebase` walkthrough row has to be able to go RED, and this file is what proves it
 * can.
 *
 * ## Why a unit test reads a QA script
 *
 * Every other assertion in this directory tests the product. This one tests the *instrument*,
 * and the reason is a fact about the row that shipped in tick 48: it read the Undo button and
 * the selection on a page on which **no reload had ever happened**. It was placed directly
 * after `drag-undo`, which had just pressed Ctrl+Z, and it never caused a conflict, never
 * clicked Reload, and never read the button again. So it returned a plausible number for a
 * graph that had never been adopted, and it would have returned the same number against a
 * `rebaseAfterReload` deleted outright.
 *
 * That is not a conservative reading. A row that cannot go red is an absent one, and the
 * cost was paid twice: three ticks cited it as evidence the rebase held.
 *
 * The second half was worse, because it looked like data. The row counted
 * `[data-selected='true']`; the cards write `data-node-selected`. So `selectedInDom` was a
 * hardcoded zero — a constant, not a measurement — and a constant in a report is the one shape
 * of wrong answer nobody suspects.
 *
 * ## What is asserted, and what is deliberately NOT
 *
 * A text search is a weak instrument and this file is honest about where it stops:
 *
 * * Every assertion below is a **structural** claim — that the row clicks the Reload button,
 *   that it re-reads the button afterwards, that it creates the conflict it needs, and that it
 *   queries the marker the product actually writes.
 * * None of it claims the row *passes* in a browser. The honest claim is the inverse one: the
 *   row is wired to the code path under test, so a defect in `rebaseAfterReload` reaches a
 *   reading instead of passing silently. The browser pass remains what proves the value.
 * * A regex cannot prove a click happened, only that the source asks for one. Where a liveness
 *   claim is impossible, the comment says so rather than dressing the search up as one — the
 *   mistake `canvas-walk.test.ts` already documents and had to guard against inside itself.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/**
 * Strip comments before searching.
 *
 * The row this file guards explains, in prose, the exact marker it used to get wrong — and a
 * search over raw source matches its own documentation. That is not a hypothetical: the first
 * draft of the marker test went red against a row that was already correct, because the fix's
 * own comment quoted the bug. A guard that trips on the sentence describing the defect cannot
 * police the defect.
 *
 * Block comments first, then line comments, and string literals are deliberately NOT stripped:
 * this file has no template literals holding a selector, and pretending otherwise would be a
 * second way for a search to answer a question nobody asked it.
 */
const stripComments = (source: string): string =>
  source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");

/** The `reload-rebase` row's own body: from its `note` back to the step that names it. */
const rowFor = (step: string): string => {
  const at = WALKTHROUGH.indexOf(`note({ step: "${step}"`);
  assert.notEqual(at, -1, `the walkthrough no longer notes a "${step}" step`);
  // Walk back to the previous top-level `// ----` banner, which is how every leg in this
  // function is separated. A window of 400 lines is ample and still bounded: the row under
  // test is the one immediately before its own note.
  const before = WALKTHROUGH.lastIndexOf("// ----", at);
  return WALKTHROUGH.slice(before === -1 ? Math.max(0, at - 4000) : before, at);
};

const RELOAD_ROW = rowFor("reload-rebase");
/** The same row with its prose removed — every SELECTOR assertion reads this one. */
const RELOAD_ROW_CODE = stripComments(RELOAD_ROW);

test("the reload row clicks the Reload button, so it measures a reload", () => {
  // The load-bearing assertion, and the one the tick-48 row would have failed hardest. Reading
  // a button is not the same as making the state the button answers for: without a conflict
  // there is no Reload control to click, and without the click there is no adoption.
  assert.ok(
    /data-save-reload/.test(RELOAD_ROW),
    "the row must press the Reload exit, not merely read a button on a page that never reloaded",
  );
});

test("the reload row creates the conflict whose banner carries the Reload button", () => {
  // Three separate things are asserted together because one fixture produces all three: a
  // second tab moves the stored version, this tab's own autosave then quotes the stale one,
  // and the resulting 409 is what renders the banner. A row that only asserted "the banner is
  // visible" would be satisfied by a fixture whose PUT was refused with `csrf_failed` and
  // therefore deleted nothing.
  assert.ok(
    /expectRefusal\(/.test(RELOAD_ROW),
    "the deliberate 409 is being produced, so it must be registered or it reads as a high finding",
  );
  assert.ok(
    /graph_version: current\.graph_version/.test(RELOAD_ROW),
    "the fixture has to move the version; a PUT quoting the version it just read is not a conflict",
  );
  assert.ok(
    /x-omnion-csrf/.test(RELOAD_ROW),
    "the fixture's PUT is cookie-authenticated and needs the CSRF header or it is refused " +
      "before the handler runs -- the fixture would delete nothing and the row would report a clean adoption",
  );
  assert.ok(
    /=== "conflict"/.test(RELOAD_ROW),
    "the row waits for the conflict banner, so it does not click Reload against a page that " +
      "never showed one",
  );
});

test("the reload row reads the Undo button AFTER the reload, not before", () => {
  // The ORDER is the assertion, and it is why this test slices the row rather than searching
  // the file: a row that clicked Reload and then read the button would also contain both
  // strings, and only their sequence distinguishes a measurement from a decoration. This is
  // the same lesson as the `reload-rebase.test.ts` ordering guard, applied to the instrument.
  //
  // It is the LAST read that matters, and the first draft of this test compared against the
  // FIRST — which is the row's own PRECONDITION read, taken before the fixture exists to prove
  // the author had an undoable edit at all. Both reads are wanted and they mean opposite
  // things: the precondition says "there was something to lose", the one after the click says
  // "there is no longer". Comparing the wrong pair made this red against a row that is right.
  const clickAt = RELOAD_ROW.indexOf("data-save-reload");
  const readAt = RELOAD_ROW.lastIndexOf("builder-undo");
  assert.notEqual(clickAt, -1, "the Reload click is missing");
  assert.notEqual(readAt, -1, "the Undo button is never read");
  assert.ok(
    clickAt < readAt,
    "the Undo button must be read after the reload: read before, the row reports the state " +
      "the page was in before the exit it claims to measure",
  );
  // And the precondition must exist, or the assertion above is satisfied by a page on which
  // the button was never enabled and there was nothing for the reload to destroy.
  assert.ok(
    /undoWasEnabledBefore/.test(RELOAD_ROW),
    "the row must also read the button BEFORE the fixture, or `undoDisabled: true` is the " +
      "answer for a history that never had an entry in it",
  );
});

test("the reload row queries the selection marker the cards actually write", () => {
  // The marker regression, stated as a test so it cannot come back. `builder-view.tsx` writes
  // `data-node-selected`; `[data-selected]` is emitted by nothing, so the count was a
  // hardcoded zero. Asserted against the PRODUCT's source as well as the row's, because a
  // selector is an assertion about the DOM and the DOM is the other file -- the same reasoning
  // the Tab-walk guard uses for `data-edge`.
  const builder = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");
  assert.ok(
    /data-node-selected=\{isSelected \? "true" : "false"\}/.test(builder),
    "the card marker this test pins the row to no longer exists; the row and the test must move together",
  );
  assert.ok(
    /data-node-selected='true'/.test(RELOAD_ROW_CODE),
    "the row must count the marker the product writes, or selectedInDom is a constant zero",
  );
  // Read the comment-stripped row, and assert on the SELECTOR's own brackets. Both details are
  // the same lesson: `[data-selected]` is a substring of `[data-node-selected]`, and both
  // spellings appear in this row's PROSE, which is the very bug being described. The first
  // draft of this test matched its own documentation and went red against a correct row.
  assert.ok(
    !/\[data-selected/.test(RELOAD_ROW_CODE),
    "the row still counts `[data-selected]`, which no card emits -- a hardcoded zero in a report",
  );
});

test("the reload row states the selection in words as well as in a class", () => {
  // A selected card's outline is a CSS class, so a selection the canvas no longer draws would
  // otherwise be visible in exactly one place -- and only to a probe that knows to look.
  // `[data-builder-selection]` is the status bar's own marker, and the product renders it only
  // when something is selected, so its ABSENCE after the reload is a reading and not a gap.
  assert.ok(
    /data-builder-selection/.test(RELOAD_ROW),
    "the row must read the status bar's selection wording, the part an author can actually see",
  );
});

test("the reload row is closed on failure rather than leaving a tab behind", () => {
  // The two-tab rows in this pass share one browser context, and an unclosed tab survives into
  // the next leg -- where it would be a live editor holding a session while the pass measures
  // a single-tab screen. The `finally` is what makes the row safe to fail, and a row that only
  // closes on success cannot fail safely.
  assert.ok(
    /finally\s*\{[\s\S]*rebaseTab\.close\(\)/.test(RELOAD_ROW),
    "the second tab must be closed in a finally, so a failed row does not leak a live editor " +
      "into the legs that follow",
  );
});

test("the reload row's whole point is the button being DISABLED -- asserted, not implied", () => {
  // `drag-undo` asserts the button ENABLES after a drag; this row asserts the opposite. Two
  // rows on one attribute in opposite directions is the pairing `state.json` asks for, and it
  // is the only shape in which a history that should not be there is visible at all: a harness
  // that only ever asks "can I undo?" cannot see it.
  assert.ok(
    /undoDisabled: after\.undoDisabled === true/.test(RELOAD_ROW),
    "the row must state the disabled reading as a boolean assertion, so a null from a missing " +
      "button is a failure rather than a nullish value that reads as a pass",
  );
  // And it must be read off the `disabled` attribute rather than a computed style, which is
  // what makes a disabled <button> legible at all.
  assert.ok(
    /hasAttribute\("disabled"\)/.test(RELOAD_ROW),
    "the reading must be the button's own disabled attribute; a computed style cannot say it",
  );
});
