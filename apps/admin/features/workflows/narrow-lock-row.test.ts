/**
 * The `narrow-lock` row's editability claim must be able to go RED, and this file is what
 * proves it can.
 *
 * ## The criterion
 *
 * "Below 1024px the builder is read-only with the banner, **Table mode stays editable**, and
 * no control is unreachable." Three claims; this file is about the middle one, because it is
 * the one that was never actually measured.
 *
 * ## The defect this file was written for (tick 86)
 *
 * The row read it with a count over two attributes the product has never rendered:
 *
 * ```js
 * const editableOnTable = await page
 *   .locator("[data-workflow-table-edit], [data-table-edit]")
 *   .count();
 * tableSaves = { landedOnTable: onTable, editControls: editableOnTable };
 * ```
 *
 * `grep -rn 'data-table-edit' apps/ scripts/ crates/` returns exactly one line — the line
 * above. Neither name exists in `table-view.tsx`, which ships `data-table-label`,
 * `data-table-param` and `data-table-save`. So `editControls` was structurally **0**: a
 * navigation that fell off a 404, and a Table mode that rendered perfectly well, reported the
 * same number.
 *
 * That is the failure every neighbouring row in this REQ had once — **the row reports a count,
 * so a constant is indistinguishable from a reading** — and it is why this file exists instead
 * of another unit test of `table-mode.ts`. `table-mode.test.ts` covers the table's rules
 * (`buildTable`, `diffTableEdits`, `toGraph`); every one of them can be correct while the row
 * that reports them reads nothing.
 *
 * It is not hypothetical that a constant hid a real defect, and the row's own comment is the
 * evidence: four lines directly above the selector say *"a lock that locked Table mode too
 * would satisfy 'read-only' and fail the criterion in the same breath. So the link is followed
 * and **a value is changed there**."* Nothing ever typed a value. The comment described the
 * measurement the row did not take, and a reader would take it for the measurement.
 *
 * ## The three rules that keep the replacement honest
 *
 * 1. **A selector must name attributes the product actually renders.** A marker that appears
 *    only inside the probe is a constant, so rule 1 checks each marker against `table-view.tsx`
 *    rather than merely documenting which ones are meant to be real.
 * 2. **"Stays editable" is a claim about *writability*, so it is measured by writing.** Counting
 *    non-disabled inputs proves the controls drew; it does not prove a keystroke reaches the
 *    draft. The row types and then reads the product's own "Unsaved changes" affordance back.
 * 3. **A zero must say *which* zero.** `tableMounted` separates "the page never rendered" from
 *    "the page rendered and every control is inert" — tick 74's lesson, and the reason a single
 *    `editControls: 0` was the wrong shape to begin with.
 */

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import assert from "node:assert/strict";
import { test } from "node:test";

const HERE = dirname(fileURLToPath(import.meta.url));
// `HERE` is apps/admin/features/workflows, so the repo root is four levels up.
const WALKTHROUGH = join(HERE, "..", "..", "..", "..", "scripts", "qa", "walkthrough.cjs");
const TABLE_VIEW = join(HERE, "table-view.tsx");

const read = (path: string) => readFileSync(path, "utf8");
const TABLE = read(TABLE_VIEW);

/**
 * Strip comments so a guard over source never fails on the prose documenting the defect.
 * `keyboard-pass-row.test.ts` had to do this for the same reason: the sentence explaining the
 * bug lives in the very file the guard reads, so a raw-source rule stays red forever against
 * correct code.
 */
function stripComments(src: string): string {
  return src
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:"'`\\])\/\/[^\n]*/g, "$1");
}
const WALK_CODE = stripComments(read(WALKTHROUGH));

/**
 * The block that navigates to Table mode on the locked screen and reads its editable surface.
 *
 * It sits BEFORE the `narrow-lock` note, so a window anchored on the note cannot reach it — the
 * tick-84 lesson about a gate's source window being part of its contract, in the one shape where
 * the window is correct and still too short. Both windows exist below for that reason.
 */
const LOCKED_TABLE_BLOCK = (() => {
  const anchor = WALK_CODE.indexOf("let tableSaves = null;");
  assert.notEqual(anchor, -1, "the locked-table block must exist in the walkthrough");
  const end = WALK_CODE.indexOf('shot(page, "page-workflow-builder-locked-table")', anchor);
  assert.ok(end > anchor, "the locked-table block must end at its screenshot");
  return WALK_CODE.slice(anchor, end);
})();

/** The row's own note: where the readings surface as fields. */
const NARROW_LOCK_NOTE = (() => {
  const start = WALK_CODE.indexOf('step: "narrow-lock"');
  assert.notEqual(start, -1, "the narrow-lock row must exist in the walkthrough");
  const close = WALK_CODE.indexOf("});", start);
  assert.ok(close > start, "the narrow-lock note must close after it opens");
  return WALK_CODE.slice(start, close);
})();

/** The object the readings are assigned to, which the note then surfaces under `tableMode`. */
const TABLE_SAVES_ASSIGNMENT = (() => {
  const anchor = WALK_CODE.indexOf("tableSaves = {", LOCKED_TABLE_BLOCK.length > 0 ? WALK_CODE.indexOf("let tableSaves = null;") : 0);
  assert.notEqual(anchor, -1, "the locked-table block must assign the tableSaves readings");
  const open = WALK_CODE.indexOf("{", anchor);
  const close = WALK_CODE.indexOf("};", open);
  assert.ok(close > open, "the tableSaves object must close after it opens");
  return WALK_CODE.slice(open, close);
})();

// --- Rule 1: the selector names attributes the product renders ---------------------------

test("the row's editability selector names attributes table-view.tsx actually renders", () => {
  // The shipped defect, spelled out, so this test fails on those bytes rather than on whatever
  // the row happens to say today.
  assert.doesNotMatch(
    LOCKED_TABLE_BLOCK,
    /data-workflow-table-edit|data-table-edit\b/,
    "the row must not read editability from attributes the product never renders — `editControls` " +
      "was structurally 0, so a 404 page and a working table reported the same number",
  );

  const selector = LOCKED_TABLE_BLOCK.match(
    /querySelectorAll\(\s*"\s*\[([^\]]*)\]"\s*\)/,
  );
  assert.ok(selector, "the row must read the editable surface with a querySelectorAll over a marker selector");

  const markers = Array.from(selector![1].matchAll(/data-table-[a-z-]+/g)).map((m) => m[0]);
  assert.ok(markers.length > 0, "the selector must name at least one data-table-* marker");
  for (const marker of markers) {
    assert.ok(
      TABLE.includes(marker),
      `the row reads \`${marker}\` but table-view.tsx never renders it — a marker that exists ` +
        "only inside the probe is a constant, not a measurement",
    );
  }
});

// A field name asserted as a bare substring is satisfied by a RENAME and by a SIBLING — both
// survivors M3 and M6 came from exactly that, and it is the tick-84 shape (a rule that passes on
// a construction it does not police). So every field below is asserted on its ASSIGNMENT
// (`/^\s*name\s*[:,=]/m`), not on the bare name.

test("the row reports rendered and editable separately, so a zero says which zero", () => {
  // `rendered` and `editable` are different facts: "the page drew no control" and "every control
  // it drew is inert" are different defects that share a count of zero.
  assert.match(
    LOCKED_TABLE_BLOCK,
    /rendered:\s*inputs\.length/,
    "the row must report how many controls rendered",
  );
  assert.match(
    LOCKED_TABLE_BLOCK,
    /editable:\s*inputs\.filter\(/,
    "the row must report how many of them are actually writable",
  );
  // The readings travel to the note under `tableMode: tableSaves`, so the fields belong to the
  // ASSIGNMENT and the note carries the link. Asserting them on the note itself would be the
  // tick-74 shape in a new place: the guard reads a window the readings never pass through.
  //
  // **Anchored on the assignment, not the substring.** A bare `tableMounted` would be satisfied
  // by `tableMountedDropped` — which is what M3 did, and it is the same hole as M6 below seen
  // from the other side. The mutation has to remove the *reading*, and a field whose name merely
  // contains it has not.
  assert.match(
    TABLE_SAVES_ASSIGNMENT,
    /^\s*tableMounted\s*,/m,
    "the readings must carry `tableMounted` — a 0 must say whether the page rendered at all",
  );
  assert.match(
    TABLE_SAVES_ASSIGNMENT,
    /^\s*editControlsRendered\s*:/m,
    "the readings must carry `editControlsRendered` beside the editable count",
  );
  assert.match(
    NARROW_LOCK_NOTE,
    /tableMode:\s*tableSaves/,
    "the note must surface the locked-table readings — a block that measures and never reports " +
      "leaves the next reader with a fix and no number",
  );
  // Asserted on the CONSTRUCT, not on its position or its spelling. M4 left the wait in place
  // behind a `void (…)`, which a substring rule reports as intact; the rule is that the mounted
  // flag is *derived from* the wait, so stripping the derivation has to fail.
  assert.match(
    LOCKED_TABLE_BLOCK,
    /const tableMounted\s*=\s*\n?\s*\(await page\s*\n?\s*\.waitForSelector\(\s*"\[data-table-mode\]"/,
    "the row must wait for the table to mount instead of sleeping a fixed duration — a fixed " +
      "delay is green against a page that has not drawn, and wrong only on a slow machine",
  );
});

test("the row proves editability by writing, not by counting inputs", () => {
  // **Order matters, because a lazy rule speaks first.** M5 removes the type by inserting an
  // early exit. The two candidate assertions are "both dispatch sites exist" and "the only exit
  // before the dispatch is the no-control guard" — and a run that trips the FIRST is red on a
  // number ("found 2") that names the symptom rather than the cause, so the mutation reports
  // itself as red-for-the-wrong-reason and the specific rule never gets to speak. The reachability
  // check comes first precisely because it is the one that says *why*.
  //
  // The TYPE is anchored on REACHABILITY, not on the dispatch merely appearing. M5 inserts
  // `if (input) return null;` after the lookup: the dispatch is still in the source, still inside
  // the same `page.evaluate`, and a `[\s\S]*?` between the call and the dispatch matches straight
  // over the early return. **A construct that is present but skipped is not a construct**, and the
  // only way to tell the difference from outside is to count the exits that could bypass it.
  const typeStart = LOCKED_TABLE_BLOCK.indexOf("const typed = await page.evaluate");
  const typeEnd = LOCKED_TABLE_BLOCK.indexOf('return { original, next, kind:', typeStart);
  assert.ok(typeStart >= 0 && typeEnd > typeStart, "the row must have a typed evaluate with a return value");
  const typeBody = LOCKED_TABLE_BLOCK.slice(typeStart, typeEnd);
  const dispatchAt = typeBody.indexOf('dispatchEvent(new Event("input"');
  assert.ok(dispatchAt > 0, "the typed evaluate must dispatch an input event");

  const exitsBeforeDispatch = typeBody.slice(0, dispatchAt).match(/return\b/g) ?? [];
  assert.equal(
    exitsBeforeDispatch.length,
    1,
    `no extra exit before the dispatch: the only one may be the \`!input\` guard — found ${exitsBeforeDispatch.length}. ` +
      "An extra exit is a path where the row renders a control and types nothing, which is exactly " +
      "the reading this rule was written to remove",
  );
  assert.match(
    typeBody,
    /if \(!input\) return null;/,
    "the single permitted exit is the no-control guard",
  );

  // Both dispatch sites must be counted, not the first one: M5's first draft stripped the type's
  // dispatch and left the RESTORE's own intact, so a substring rule stayed green on a row that no
  // longer typed anything.
  const dispatches = LOCKED_TABLE_BLOCK.match(/dispatchEvent\(new Event\("input"/g) ?? [];
  assert.ok(
    dispatches.length >= 2,
    `the row must dispatch an input event in BOTH the type and the restore — found ${dispatches.length}. ` +
      "Stripping one of two sites leaves the rule satisfied by the half it was meant to police",
  );
  assert.match(
    LOCKED_TABLE_BLOCK,
    /data-table-save-state/,
    "the row must read the product's own save-state affordance back",
  );
  assert.match(
    TABLE_SAVES_ASSIGNMENT,
    /^\s*typedAccepted\s*,/m,
    "the readings must carry `typedAccepted`, or the write is invisible in the report",
  );
  assert.match(
    TABLE_SAVES_ASSIGNMENT,
    /^\s*saveEnabledAfterTyping\s*,/m,
    "the readings must carry `saveEnabledAfterTyping` — the product agreeing a draft is " +
      "committable is a separate claim from the draft existing",
  );
});

test("the probe restores the author's own value, and restores it from the value it saved", () => {
  // This row is about the lock; a probe that left a dirty draft on a real rule would make every
  // later row's `graph_version` reading a statement about the probe rather than about the product.
  assert.match(
    LOCKED_TABLE_BLOCK,
    /const original = input\.value/,
    "the row must capture the value before typing it",
  );
  // `page.evaluate(fn, arg)` is the only shape that puts the saved value back into the browser. A
  // restore that re-reads the DOM restores the probe's own value, which is a no-op.
  assert.match(
    LOCKED_TABLE_BLOCK,
    /page\.evaluate\([\s\S]{0,600}?, typed\.original\)/,
    "the restore must pass `typed.original` into the page — a restore that re-reads the input " +
      "restores the probe's own value and the draft stays dirty",
  );
});

// --- Rule 3: the readings keep the fields earlier reports cited --------------------------

test("the readings keep the fields earlier reports cited", () => {
  // A silent rename would make two numbers in two reports look like one measurement, and the
  // next reader would have no way to tell an old structural 0 from a new reading.
  //
  // **M6 was satisfied by a sibling, not a rename.** `editControlsRendered` sits on the very next
  // line and CONTAINS `editControls`, so a substring rule reported the field present after it was
  // renamed to `editableControls`. The field and its count-of-rendered twin are different facts;
  // only the assignment anchor tells them apart.
  assert.match(
    TABLE_SAVES_ASSIGNMENT,
    /^\s*editControls\s*:/m,
    "the readings must keep `editControls`; renaming the field hides which earlier reports cited the constant",
  );
  assert.match(
    TABLE_SAVES_ASSIGNMENT,
    /^\s*landedOnTable\s*[:,=]/m,
    "the readings must keep `landedOnTable` — it separates a real table page from a 404",
  );
});