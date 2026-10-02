import { strict as assert } from "node:assert";
import test from "node:test";

import {
  EDITING_CONTROLS,
  EDITOR_MIN_WIDTH,
  LOCK_BANNER,
  builderLayoutClass,
  controlAt,
  isEditorLocked,
  isFullyEditable,
  isReadingKey,
  lockPlan,
} from "./viewport-lock.ts";

test("the breakpoint is the one the layout was designed at, and the edge belongs to the editor", () => {
  assert.equal(EDITOR_MIN_WIDTH, 1024);
  // The classic off-by-one: a media query of `min-width: 1024px` and a comparison of `<= 1024`
  // disagree at exactly one width, and the screenshot taken at that width proves the JS is
  // wrong. `>= 1024` is editable on both sides.
  assert.equal(isEditorLocked(1023), true);
  assert.equal(isEditorLocked(1024), false);
  assert.equal(isEditorLocked(1025), false);
});

test("a phone is locked and a desktop is not", () => {
  assert.equal(isEditorLocked(390), true, "a 390px iPhone must not get the canvas");
  assert.equal(isEditorLocked(1440), false);
  assert.equal(isFullyEditable(1440), true);
  assert.equal(isFullyEditable(390), false);
});

test("every declared control is accounted for, so a new one cannot be added and forgotten", () => {
  const plan = lockPlan(390);
  for (const control of EDITING_CONTROLS) {
    assert.ok(plan[control], `${control} has no entry in the lock plan`);
  }
  assert.equal(Object.keys(plan).length, EDITING_CONTROLS.length);
});

test("a locked builder hides the pointer-precision mutations rather than greying them", () => {
  for (const control of ["add-node", "move-node", "connect", "delete", "edit-parameter"] as const) {
    const locked = controlAt(390, control);
    assert.equal(locked.hidden, true, `${control} must not be a focusable dead button`);
    assert.match(locked.reason, /larger screen|Table mode/i, `${control} must say why`);
  }
});

test("a disabled-but-visible control states a reason, because greyed-out is not an explanation", () => {
  const layout = controlAt(390, "auto-layout");
  assert.equal(layout.hidden, false, "a reader may ask whether the graph is laid out sensibly");
  assert.match(layout.reason, /larger screen/i);
  assert.notEqual(layout.reason, "", "an empty reason on a disabled control is a dead button");
});

test("history is hidden while locked, because there is nothing in this tab to undo", () => {
  const history = controlAt(390, "history");
  assert.equal(history.hidden, true);
  assert.match(history.reason, /nothing to undo/i);
});

test("wide means every control is enabled with no reason text at all", () => {
  for (const control of EDITING_CONTROLS) {
    const wide = controlAt(1600, control);
    assert.equal(wide.hidden, false);
    assert.equal(wide.reason, "", `${control} must carry no reason when it is enabled`);
  }
});

test("a lock without a way out is a dead end, and the banner is that way out", () => {
  assert.match(LOCK_BANNER.title, /larger screen/i);
  assert.match(LOCK_BANNER.body, /Table mode stays editable/i);
  assert.equal(LOCK_BANNER.tableModeLabel, "Open Table mode");
  // The body promises an escape; the label has to be a real one.
  assert.notEqual(LOCK_BANNER.tableModeLabel.trim(), "");
});

test("the layout class flips exactly where the lock does, never a pixel apart", () => {
  for (const width of [320, 390, 768, 1023, 1024, 1440, 2560]) {
    assert.equal(
      builderLayoutClass(width).includes("locked"),
      isEditorLocked(width),
      `class and lock disagree at ${width}px`,
    );
  }
  assert.equal(builderLayoutClass(390), "builder-locked");
  assert.equal(builderLayoutClass(1440), "builder-wide");
});

test("reversing the comparison is a red test, because that is the bug this file exists for", () => {
  // The classic error is `<= EDITOR_MIN_WIDTH`. Asserted negatively so a later "tidy-up" of
  // the boundary cannot silently unlock one pixel too early.
  const wrong = (width: number) => width <= EDITOR_MIN_WIDTH;
  assert.equal(wrong(1024), true);
  assert.equal(isEditorLocked(1024), false, "the two must differ at the edge");
});

test("a hardware keyboard on a locked screen cannot mutate the graph", () => {
  // The hole a pointer-only lock leaves: a phone with a Bluetooth keyboard gets every
  // shortcut on a screen the banner calls read-only.
  for (const key of ["Delete", "Backspace", "Enter"]) {
    assert.equal(isReadingKey({ key }), false, `${key} must be refused while locked`);
  }
  // Every ⌘ chord in the builder is a write, including the innocuous-looking ones.
  for (const key of ["z", "c", "v", "d", "s", "a", "p"]) {
    assert.equal(isReadingKey({ key, metaKey: true }), false, `⌘${key} must be refused`);
    assert.equal(isReadingKey({ key, ctrlKey: true }), false, `Ctrl+${key} must be refused`);
  }
});

test("navigation stays open while locked, or the lock is a screenshot", () => {
  for (const key of [
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "Tab",
    "Escape",
    "Home",
    "End",
    " ",
  ]) {
    assert.equal(isReadingKey({ key }), true, `${key} must keep working while locked`);
  }
});

test("a key the whitelist has never heard of is refused, not allowed", () => {
  // The whitelist is the safe default: a shortcut added next year mutates by default and
  // has to be added here deliberately, rather than being exempted by omission.
  for (const key of ["c", "v", "r", "i", "p", "0", "+", "-", "F2"]) {
    assert.equal(isReadingKey({ key }), false, `${key} must be refused while locked`);
  }
});
