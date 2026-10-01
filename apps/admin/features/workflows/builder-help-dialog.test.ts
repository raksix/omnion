/**
 * The ⌘/ shortcut list is a `role="dialog"` that claims — in its own source comment — "Escape
 * closes it", and tick 75's pass read `closedByEscape: false` off a live builder.
 *
 * ## Why that reading is a product defect and not a probe defect
 *
 * Every other row in this REQ's history that read red was a **probe** asking the wrong question
 * (`node.node_type`, `/graph/validate`, a fixed sleep). This one is the other kind, and the shape
 * is worth stating precisely because it is new to this file:
 *
 * The Escape handler lives in `onCanvasKeyDown`, which is React's `onKeyDown` on the **canvas
 * div**. The help overlay is a *sibling* of that div, not a descendant, so a key pressed while
 * focus is inside the dialog never bubbles through the canvas. The dialog therefore inherits the
 * canvas's focus — it has no `autoFocus`, no focus trap, and no listener of its own — and
 * **Escape closes the dialog only when the canvas happened to hold focus** at the moment the key
 * was pressed.
 *
 * So the trap is real and reachable, and this REQ's own history says which gestures move focus:
 * `⌘P` focuses the palette (`focusPaletteItem`), `I` focuses the inspector (`focusInspector`),
 * and the palette rail and the inspector are both siblings of the canvas. An author who presses
 * `⌘/`, then tabs or presses `I`, is in a `role="dialog"` with `aria-modal="true"` whose only
 * documented exit does not work.
 *
 * The tick-75 note says "the row is measuring a focus-dependent product" without naming the
 * dependency. The assertion below names it and makes it red-able.
 *
 * ## What is asserted, and what is deliberately NOT
 *
 * * **Reachability before verdict.** As in `plugin-palette-row.test.ts`: the row's reading is only
 *   meaningful if the probe can open the dialog at all, so `opened` stays its own field.
 * * **The guard is structural and lives next to the product**, because the failure is a *tree
 *   shape* (sibling, not descendant) that a unit test reading strings can only approximate. The
 *   two assertions that matter are "the dialog owns an Escape handler of its own" and "the dialog
 *   takes focus when it opens".
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const BUILDER = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");

/** Block comments first, then line comments; string literals deliberately NOT stripped. */
const stripComments = (source: string): string =>
  source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");

const CODE = stripComments(BUILDER);

/**
 * The offset of the DIALOG's own `data-builder-help`, as opposed to its backdrop's.
 *
 * **Two drafts of this anchor were wrong before this one, and both failures reported the product
 * as broken.**
 *
 * 1. The bare string `data-builder-help` also matches `data-builder-help-backdrop`, so the window
 *    opened on the backdrop and every assertion was answered by the wrong element.
 * 2. The fix was `data-builder-help"` — but the attribute is written **bare** (`data-builder-help`
 *    with no value, as JSX allows), so there is no closing quote anywhere in the file and
 *    `indexOf` returned `-1`.
 *
 * The anchor is therefore a **negative lookahead**, not a delimiter: the attribute's own end is
 * "the next character is not `-` and not a word character". That is the same rule the attribute's
 * spelling requires, so it cannot drift when the markup is reformatted — and both failure modes
 * above become one that is checkable rather than two that are remembered.
 *
 * `assert.diagnosticTagAt` is asserted, not assumed: it is the difference between a window that
 * opened on the dialog and one that opened on the backdrop.
 */
const dialogAttrAt = (): number => {
  const match = /data-builder-help(?![\w-])/.exec(CODE);
  assert.ok(match, "the help dialog must still be rendered");
  return match.index;
};

/**
 * The dialog's own opening tag.
 *
 * The window ends at the tag's own `>`, so an assertion about it cannot be satisfied by a handler
 * elsewhere in the file — the trap this file exists for is precisely a handler that lives
 * somewhere *else* and happens to work when focus cooperates.
 */
const dialogTag = (): string => {
  const at = dialogAttrAt();
  const open = CODE.lastIndexOf("<div", at);
  assert.notEqual(open, -1, "the dialog must still be a div");
  const close = CODE.indexOf(">", at);
  assert.notEqual(close, -1, "the dialog's opening tag must close");
  const tag = CODE.slice(open, close + 1);
  // The element found must be the dialog itself. Without this the anchor is a bare attribute name
  // again and every assertion below is about whichever element happens to carry it.
  assert.match(
    tag,
    /role="dialog"/,
    "the element carrying data-builder-help must be the dialog, not the backdrop",
  );
  return tag;
};

/**
 * The dialog's BODY: from its opening tag to the end of the `helpOpen ? (` block, so an
 * assertion about "the dialog has no key handler" is not satisfied by the canvas's, which sits
 * ~1,700 lines below.
 */
const dialogBlock = (): string => {
  const at = dialogAttrAt();
  const blockStart = CODE.lastIndexOf("{helpOpen ? (", at);
  assert.notEqual(blockStart, -1, "the dialog must be mounted from the `helpOpen` conditional");
  // The block is `{helpOpen ? ( … ) : null}`. The closing `) : null}` is the only one at this
  // nesting level, and searching for it from the block start cannot run past into the next
  // sibling — it is the same `indexOf` shape the sibling guard files use, applied to JSX.
  const end = CODE.indexOf(") : null}", blockStart);
  assert.notEqual(end, -1, "the helpOpen conditional must close");
  return CODE.slice(blockStart, end + ") : null}".length);
};

test("the dialog owns an Escape handler, not the canvas it is not inside", () => {
  // **The load-bearing assertion.** A dialog that renders `role="dialog"` + `aria-modal="true"`
  // and relies on an ancestor's key handler is a modal with no keyboard exit, and it only
  // "works" while focus happens to sit where that ancestor can hear it. The probe's own comment
  // says the list is opened from the canvas, so on the happy path the canvas *does* hold focus —
  // which is why the defect survived a reading of `false` that a reader could explain away as
  // timing.
  //
  // `stopPropagation` is asserted alongside: a dialog that handles Escape but lets the event
  // continue is a dialog where one press closes the list AND runs the canvas's Escape ladder
  // (cancel a connection, clear an edge selection, clear the node selection) behind it.
  const block = dialogBlock();
  assert.match(
    block,
    /onKeyDown=/,
    "the dialog must carry its own onKeyDown — a sibling canvas cannot hear Escape for it",
  );
  assert.match(
    block,
    /event\.key\s*!?==?\s*"Escape"/,
    "the dialog must claim Escape itself, or its only documented exit is focus-dependent",
  );
  // **This assertion was satisfied by the wrong call, and M2 is what proved it.** The backdrop
  // carries `onClick={(click) => click.stopPropagation()}` — a *pointer* handler, on a different
  // element, for a different event — and the block-level check counted both. Removing the keydown's
  // `stopPropagation` left the suite green: an assertion about "the dialog consumes Escape" was
  // being answered by "the backdrop does not let a click reach the page".
  //
  // So the claim is read from **inside the keydown handler**, not from the block: the window is
  // the handler's own `onKeyDown={(event) => { … }}` body. The two handlers cannot be confused
  // for one another again, and the wording of the failure names the event.
  const handler = block.slice(block.indexOf("onKeyDown="), block.indexOf("})", block.indexOf("onKeyDown=")));
  assert.match(
    handler,
    /stopPropagation\(\)/,
    "the dialog's KEYDOWN must consume Escape: one press must not also run the canvas's own Escape ladder",
  );
  assert.match(
    handler,
    /preventDefault\(\)/,
    "the dialog must preventDefault on Escape, or the browser's own dismissal races the state change",
  );
});

test("the dialog takes focus when it opens", () => {
  // The half that makes the handler above reachable *in practice*. A dialog that listens for
  // Escape but never receives focus is the same trap wearing a different hat — the key goes to
  // `document.activeElement`, which is still the canvas or a palette button, so the handler is
  // correct and unreachable.
  //
  // **The focusable control inside the dialog, not the dialog's own tag — the first draft had
  // this backwards and reported the product as broken.** A `<div role="dialog">` is not
  // focusable without `tabIndex`, so asserting `autoFocus` on the dialog's tag can never pass on
  // any implementation; the honest claim is that *something inside it* takes focus. The panel is
  // a close button plus a static list, so the close button is the only candidate, and letting the
  // platform focus it is the whole fix — no ref, no effect, no restore-focus bookkeeping.
  //
  // The window is the dialog's opening tag through the end of its header row, so an `autoFocus`
  // elsewhere in the builder (and there are `focus()` calls at 1455 and 1661) cannot satisfy it.
  const block = dialogBlock();
  const header = block.slice(0, block.indexOf("data-help-row") === -1 ? block.length : block.indexOf("data-help-row"));
  assert.match(
    header,
    /autoFocus/,
    "a control inside the dialog must take focus when it opens, or its Escape handler is unreachable",
  );
  // **Two of this test's assertions were the wrong ones, and M4/M5 are what proved it.** With
  // `autoFocus` deleted, the suite went red — but on the `<button>` check below, not on the "the
  // dialog must focus a control" claim, because that earlier assertion was a `doesNotMatch` and
  // removing an attribute cannot fail it. **A presence assertion and an absence assertion cannot
  // police the same attribute**: whoever is deleted, one of them is guaranteed to stay green, so
  // the pair only ever proves that *something* about the attribute held. Both are now positive
  // claims about a located attribute.
  const focusAt = header.indexOf("autoFocus");
  assert.notEqual(focusAt, -1, "the dialog must focus a control when it opens");
  // The element carrying it must be a button, read BACKWARDS from the attribute: the window
  // between `<button` and its `autoFocus` is genuinely open-ended, because an `onClick` arrow's
  // `=>` contains a `>` — which is why the forward `<button[^>]*autoFocus` (my first guess) can
  // never match any correct markup. Splitting the claim in two is what makes it checkable without
  // either half needing to know the other's attribute order.
  const buttonAt = header.lastIndexOf("<button", focusAt);
  assert.notEqual(
    buttonAt,
    -1,
    "the focused control must be a `<button>`: `autoFocus` on a non-focusable element is a no-op",
  );
  // M5 moved the attribute onto the dialog's own tag, where the control above no longer exists.
  // This is the assertion that says so: the attribute must sit INSIDE the dialog's children, so
  // the element immediately preceding it cannot be the element that *is* the dialog.
  const preceding = header.slice(0, buttonAt);
  assert.match(
    preceding,
    />\s*$/,
    "the focused control must be a child element of the dialog, not the dialog tag itself",
  );
  assert.doesNotMatch(
    preceding.slice(preceding.lastIndexOf("<")),
    /role="dialog"|data-builder-help/,
    "autoFocus must be on a control INSIDE the dialog, not on the dialog's own tag",
  );
  // The close button is the dialog's way out with the pointer, so the thing focus lands on and the
  // thing a mouse author clicks must be the same control — an author who tabs in and then presses
  // Enter should leave, not activate a node behind the dialog. Asserted within the button's own
  // tag, which is short and therefore safe to match across.
  const buttonTag = header.slice(buttonAt, header.indexOf(">", focusAt) + 1);
  assert.match(
    buttonTag,
    /onClick=\{\(\) => setHelpOpen\(false\)\}/,
    "the focused control must be the same one that closes the dialog on click",
  );
});

test("the palette and inspector are siblings of the canvas, so this trap is reachable", () => {
  // **The reachability half, and the reason the defect was believed to be a timing artefact.**
  //
  // If the canvas were an ancestor of every focusable region, the trap would need a probe to hit
  // and a browser to reach. It is the opposite: `⌘P` focuses the palette (`focusPaletteItem`),
  // `I` focuses the inspector (`focusInspector`), and both are siblings of the canvas div. So the
  // sequence "press ⌘/, then Tab (or I)" — three keystrokes, no mouse — lands focus outside the
  // canvas with a modal open, and Escape stops working.
  //
  // Asserted as a *negative* about the tree shape rather than as a positive about focus calls:
  // the point is that nothing can make the canvas an ancestor, so no future refactor fixes this
  // by accident.
  const canvasAt = CODE.indexOf("onKeyDown={onCanvasKeyDown}");
  assert.notEqual(canvasAt, -1, "the canvas must still carry the key handler");
  const paletteAt = CODE.indexOf("data-builder-palette");
  // **The inspector attribute's FIRST occurrence is inside a `querySelector` string**, in
  // `focusInspector` — not on an element at all. So `indexOf` lands in the middle of code, the
  // derived window walks back to some unrelated `<`, and the assertion was green for a tag that
  // does not exist. Three wrong anchors in this one file (backdrop, then `data-builder-inspector`
  // in a query string, then `<aside` for an element that is not an aside), all of them reporting
  // the product as broken.
  //
  // The anchor that survives all three: the attribute must be a JSX ATTRIBUTE, so it stands alone
  // on its own line, indented, with nothing but whitespace after it. A string literal inside code
  // is never indented like that — `querySelector("[data-builder-inspector]")` keeps it on the same
  // line as the call.
  //
  // **The first draft demanded `/>` or `>` right after the name and matched nothing**, because
  // this attribute is followed by a comment explaining why the rail is not inert when nothing is
  // selected. That is the fourth wrong anchor in this file, and it failed the same way as the
  // others: it reported the product as missing an element it plainly has. So the tail is "nothing
  // but whitespace until the end of the line", which is what an attribute on its own line means and
  // what no inline string is.
  const inspectorMatch = /\n\s+data-builder-inspector\s*$/m.exec(CODE);
  assert.ok(inspectorMatch, "the inspector must still exist as an attribute on an element");
  const inspectorAt = inspectorMatch.index + 1;
  assert.ok(paletteAt > 0, "the palette rail must still exist");
  assert.ok(inspectorAt > 0, "the inspector must still exist");

  // Each region is a distinct sibling: the handler's host is not an ancestor of any of them, so a
  // key pressed inside one of them cannot reach `onCanvasKeyDown`.
  //
  // **The window is the region's OWN opening tag, and M7 is what proved the old one could not be.**
  // `CODE.slice(at - 400, at + 40)` is a fixed character budget around the attribute, which is a
  // claim about *distance* — and the palette's tag is longer than 400 characters (it carries four
  // `inert` comments' worth of attributes), so an `onKeyDown` added at the very top of that tag
  // fell outside the window and the suite stayed green. A character budget is a guess about how
  // long a tag is; the tag's own delimiters are not.
  //
  // So the window is derived: back up to the element's `<`, forward to the `>` that closes its tag.
  // And the assertion it makes is stronger than the one it replaced: a region may legitimately
  // have `onKeyDown` on a CHILD — the palette's own buttons answer Enter — so the claim is about
  // the root tag only, and the root tag is what "sibling" means here.
  const rootTagOf = (attributeAt: number): string => {
    const open = CODE.lastIndexOf("<", attributeAt);
    const close = CODE.indexOf(">", attributeAt);
    assert.ok(open > 0 && close > open, `${attributeAt} must sit inside a tag the window can bound`);
    return CODE.slice(open, close + 1);
  };

  for (const [name, at] of [
    ["palette", paletteAt],
    ["inspector", inspectorAt],
  ] as const) {
    const rootTag = rootTagOf(at);
    // The window must have opened on the element that CARRIES the attribute — a `querySelector`
    // string, or a child that merely mentions it, would otherwise pass for the region's root.
    // Asserted as presence of the attribute in the very tag the window returned, which is the
    // claim "the sibling is the region itself" rather than "something nearby has this name".
    assert.ok(
      rootTag.includes(name === "palette" ? "data-builder-palette" : "data-builder-inspector"),
      `${name}'s own root tag must carry its data attribute, not a neighbour's`,
    );
    assert.doesNotMatch(
      rootTag,
      /onKeyDown=/,
      `${name} must not carry onKeyDown — the canvas handler is its SIBLING's, and a key inside it never bubbles there`,
    );
  }

  // And the dialog itself must not be nested inside the canvas's tag, which is the only shape in
  // which the canvas's handler would have been correct all along. Both offsets must be real — an
  // assertion comparing two values where one is `-1` passes vacuously, which is how the first
  // draft of this test said nothing while looking like it said something.
  assert.ok(
    canvasAt > 0 && dialogAttrAt() > 0,
    "both the canvas handler and the dialog must be present for this comparison to mean anything",
  );
  assert.ok(
    dialogAttrAt() < canvasAt,
    "the dialog is rendered before the canvas in the tree; if that stops being true the sibling claim must be re-derived",
  );
});

test("the source comment claims Escape closes it, and the claim must stay true", () => {
  // **This file is the guard for a stated contract.** The dialog's own comment already said "It
  // is a `dialog` and it traps nothing — Escape closes it" — a claim that was FALSE for every
  // author whose focus had left the canvas, and no gate anywhere read it.
  //
  // The assertion is deliberately weak (the phrase must survive) and deliberately loud: it names
  // the condition under which the claim becomes false, so the next writer reading this file learns
  // that the sentence is a contract with a guard, not a description.
  assert.match(
    BUILDER,
    /Escape closes it/,
    "the dialog's own comment states the contract this file enforces; removing it is a real edit",
  );
  // The comment is stripped for CODE assertions, so a fix that only edits prose cannot pass the
  // tests above — asserted here so the two halves cannot be confused for one another.
  assert.doesNotMatch(
    CODE,
    /Escape closes it/,
    "the contract sentence must live in a comment, so stripping comments removes it from CODE",
  );
});
