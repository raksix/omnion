/**
 * The Tab walk (REQ-004, slice 4).
 *
 * The test that matters is the last one. The defect this file exists for was a **documented
 * shortcut with no handler**, and the drift guard that should have caught it had no arm for
 * that shape: it walked the handler and asked whether every chord had a row, which a row for
 * a key nothing implements passes. So the guard added here is the missing arm — it reads the
 * canvas source and fails on a row whose key the handler never binds.
 *
 * That guard is proved by injection, like the shortcut catalogue's: the binding is removed,
 * the test names the chord, and the file is restored. A guard nobody has seen fail is a
 * comment with a test runner attached.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { isNodeSelected, type CanvasSelection } from "./selection.ts";
import {
  isEdgeIn,
  nextFocusable,
  shouldWalkCanvas,
  walkSelection,
} from "./canvas-walk.ts";
import { SHORTCUT_ROWS } from "./keyboard-path.ts";

const world = {
  nodes: ["n1", "n2", "n3"],
  edges: ["e1"],
};

const focused = (id: string | null): CanvasSelection => ({ nodes: [], focus: id, edge: null });

test("Tab walks forward from the focused card and wraps", () => {
  assert.equal(nextFocusable(world, focused("n1")), "n2");
  assert.equal(nextFocusable(world, focused("n2")), "n3");
  // Wrapping is the point: a walk that stopped at the end would need an End key, and the
  // canvas has no other way to get back to the first card.
  assert.equal(nextFocusable(world, focused("n3")), "e1");
  assert.equal(nextFocusable(world, focused("e1")), "n1");
});

test("Shift+Tab walks the same rotation backwards", () => {
  // The same rule, not a second one. A separate "backwards scan" is where an off-by-one at
  // the wrap point lives, and the wrap point is the only place it shows.
  assert.equal(nextFocusable(world, focused("n2"), "backward"), "n1");
  assert.equal(nextFocusable(world, focused("n1"), "backward"), "e1");
  assert.equal(nextFocusable(world, focused("e1"), "backward"), "n3");
  assert.equal(nextFocusable(world, focused("n3"), "backward"), "n2");
});

test("the two directions are inverses across every step of the walk", () => {
  // An exhaustive version of the three above: a rotation whose backward half is not the
  // inverse of its forward half is still a plausible-looking walk, and only a loop over
  // every id finds the one step where it disagrees.
  for (const id of [...world.nodes, ...world.edges]) {
    const forward = nextFocusable(world, focused(id), "forward");
    const back = nextFocusable(world, focused(id), "backward");
    assert.notEqual(forward, id, `${id} walked onto itself`);
    assert.equal(
      nextFocusable(world, focused(forward ?? null), "backward"),
      id,
      `walking forward from ${id} to ${forward} does not come back to ${id}`,
    );
    assert.equal(
      nextFocusable(world, focused(back ?? null), "forward"),
      id,
      `walking backward from ${id} to ${back} does not come back to ${id}`,
    );
  }
});

test("with nothing focused, forward starts at the first card and back at the last", () => {
  assert.equal(nextFocusable(world, focused(null), "forward"), "n1");
  assert.equal(nextFocusable(world, focused(null), "backward"), "e1");
});

test("a focus that names a deleted node restarts the walk rather than doing nothing", () => {
  // The phantom-focus case. Returning `null` here would look like "Tab does nothing", which
  // is the exact symptom of the bug this module fixes — a dead key reading as a dead key.
  // Landing on the first card at least tells the author the selection moved.
  const ghost = focused("deleted-9");
  assert.equal(nextFocusable(world, ghost), "n1");
  assert.equal(nextFocusable(world, ghost, "backward"), "e1");
});

test("an empty graph has nothing to walk, and says so with null", () => {
  const empty = { nodes: [], edges: [] };
  assert.equal(nextFocusable(empty, focused(null)), null);
  assert.equal(nextFocusable(empty, focused("n1")), null);
});

test("the walk resumes from the focus even when a group is outlined", () => {
  // A marquee or Shift+click leaves several cards outlined with the focus on one. Continuing
  // from the group would skip cards, and continuing from the group's *first* member would
  // jump backwards from where the inspector is showing.
  const group: CanvasSelection = { nodes: ["n1", "n3"], focus: "n2", edge: null };
  assert.equal(nextFocusable(world, group), "n3");
});

test("a walk replaces the selection instead of accumulating it", () => {
  // Tab is a move. Six presses must not leave six outlined cards whose `Del` would take all
  // of them — that would make the arrow-nudge (which moves the drawn selection) move
  // something the author did not intend to move.
  const start: CanvasSelection = { nodes: ["n1", "n2"], focus: "n3", edge: null };
  const after = walkSelection(world, start);
  assert.deepEqual(after, { nodes: [], focus: "e1", edge: null });
  assert.equal(isNodeSelected(after, "n1"), false, "the group survived the walk");
  assert.equal(isNodeSelected(after, "e1"), true);
});

test("a walk onto an edge is recognisable as an edge", () => {
  // The walk lands on edges as well as nodes (`focusOrder` puts them last), and a caller
  // that could not tell the difference would select a line the outline does not draw.
  const after = walkSelection(world, focused("n3"));
  assert.equal(isEdgeIn(world, after.focus), true);
  assert.equal(isEdgeIn(world, nextFocusable(world, focused("n1"))), false);
  assert.equal(isEdgeIn(world, null), false);
});

test("Tab in a field is the field's, and the walk refuses it", () => {
  // The failure this guards is not theoretical: `I` focuses the inspector's first input, and
  // a Tab that jumped the selection instead of leaving the field would make the criterion's
  // "edits a parameter" step impossible — while looking like the shortcut was broken.
  //
  // The targets are cast, because the real caller hands a React event whose `target` is an
  // `EventTarget`. Casting in the *test* is legitimate; casting at the call site to satisfy a
  // narrower signature would not be, which is why `shouldWalkCanvas` takes the wide type.
  const asTarget = (tagName: string) => ({ tagName }) as unknown as EventTarget;
  for (const tagName of ["INPUT", "TEXTAREA", "SELECT"]) {
    assert.equal(
      shouldWalkCanvas({ key: "Tab", target: asTarget(tagName) }),
      false,
      `${tagName} lost its own Tab`,
    );
  }
  assert.equal(
    shouldWalkCanvas({
      key: "Tab",
      target: { isContentEditable: true } as unknown as EventTarget,
    }),
    false,
    "a contentEditable field lost its own Tab",
  );
  // And a target the predicate cannot read is NOT a typing target. Defaulting to "refuse"
  // would make the walk dead on any event whose target the function cannot inspect, which is
  // a key that does nothing — the exact defect this module fixes.
  assert.equal(shouldWalkCanvas({ key: "Tab" }), true);
  assert.equal(shouldWalkCanvas({ key: "Tab", target: null }), true);
  assert.equal(shouldWalkCanvas({ key: "Tab", shiftKey: true }), true);
  // And the guard is not simply "any key is a Tab" — a stray Escape must not walk.
  assert.equal(shouldWalkCanvas({ key: "Escape" }), false);
  assert.equal(shouldWalkCanvas({ key: "Enter" }), false);
});

test("every row in the shortcut list names a key the canvas actually handles", () => {
  // The missing arm of the drift guard, and the defect it was written for.
  //
  // The existing guard walks `onCanvasKeyDown` and asks "does every chord it binds have a
  // row?", which is the other direction. This asks the question that would have caught the
  // `Tab` row: **does every row name something real?** A help list is the one screen whose
  // value is exactly as fresh as its last edit — a row for a key nothing implements is the
  // list lying, and the author has no way to tell it from a row that works.
  //
  // `keys` is prose ("Arrows", "⇧⌘Z or ⌘Y", "Del"), so a row is checked by the *key names it
  // mentions* rather than by string equality: each name must be bound somewhere in the
  // handler or be a key the browser owns on its own element. Names that are pure prose
  // ("Arrows") name a family, and the family is what has to exist.
  const source = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");
  const handler = source.slice(
    source.indexOf("const onCanvasKeyDown"),
    source.indexOf("// ---- actions"),
  );

  /** `Del` is spelled "Delete" in the DOM, `Esc` as "Escape"; both are handled as written. */
  const ALIASES: Record<string, string[]> = {
    esc: ["escape"],
    del: ["delete", "backspace"],
    enter: ["enter"],
    tab: ["tab"],
    arrows: ["arrowup", "arrowdown", "arrowleft", "arrowright"],
    space: ["space"],
  };

  /** Split a row's label into the key names it mentions, lower-cased. */
  const namesIn = (keys: string) =>
    keys
      .toLowerCase()
      .split(/[^a-z]+/)
      .filter((name) => name.length > 1 && name !== "or");

  // A key can be handled *literally* (`event.key === "Delete"`), **delegated** to a
  // predicate in another module (`readKey` for the single-key path, `shouldWalkCanvas` for
  // the Tab walk), or **table-indexed** (`nudge[event.key]` for the arrows). A guard that only
  // text-searches for `event.key ===` reads all three shapes as unbound, which is how the
  // first two versions of this test failed — and the shape of the failure is the point: each
  // time the *instrument* was blind, not the product. The three sources below are the three
  // ways a key can legitimately be bound, and each is anchored to its own syntax so an
  // unrelated object literal cannot inject a phantom key.
  //
  // The delegation only counts if the handler really calls it. Asserting that is what stops
  // this being a rubber stamp: a refactor that removed the `readKey` call would drop every
  // `KEYMAP` key out of the handled set and go red on `Enter`, `C`, `V`, `R`, `I` and `P` at
  // once, which is exactly the alarm it should raise.
  const walkSource = readFileSync(new URL("./canvas-walk.ts", import.meta.url), "utf8");
  assert.ok(
    /from "\.\/canvas-walk(\.ts)?"/.test(source),
    "the shortcut list advertises Tab but the canvas no longer imports the walk that handles it",
  );
  // **The binding must be reachable, not merely present.** The first version of this
  // assertion was `/shouldWalkCanvas\(/`, and an injection proved it a comment: gating the
  // case on a constant false (`if (NEVER_TRUE && shouldWalkCanvas(event))`) left the call's
  // text in the file and the guard passed — 11/11 green on a Tab that does nothing. That is
  // the exact shape of the bug this test exists for, reproduced *inside the instrument*, and
  // it is why a text search cannot be the whole guard.
  //
  // So the check is that the call is the *condition* of an `if`, not merely somewhere in the
  // body: a dead `&&` branch changes the text before the call but not the `if (`. That is a
  // syntactic reading, not a proof of liveness, and the honest form of the claim is stated
  // above it rather than overclaimed — the browser pass remains what proves Tab moves a
  // selection, and this guard is what stops the binding from being *removed*.
  assert.ok(
    /if \(\s*shouldWalkCanvas\(event\)\s*\)/.test(handler),
    "the canvas imports the Tab walk but never routes a key to it, so Tab does nothing",
  );
  const keymapSource = readFileSync(new URL("./keyboard-path.ts", import.meta.url), "utf8");
  const keymap = keymapSource.slice(
    keymapSource.indexOf("export const KEYMAP"),
    keymapSource.indexOf("} as const;"),
  );

  const handlerKeys = new Set(
    [
      ...handler.matchAll(/event\.key\s*===\s*"([^"]+)"/g),
      ...handler.matchAll(/event\.key\.toLowerCase\(\)\s*===\s*"([^"]+)"/g),
      // The **table-indexed** shape: the arrow keys are keys of a
      // `Record<string, [number, number]>` the handler indexes with `nudge[event.key]`, not
      // comparisons. A text search for `event.key ===` cannot see a table lookup, so without
      // this the "Arrows" row reads as advertised-but-unbound — the very defect the test
      // exists for, now in the *instrument*. Anchored to the nudge table's own shape so a
      // random object literal cannot add phantom keys.
      ...handler.matchAll(/^\s+(Arrow\w+|Page\w+|Home|End|Enter|Tab|Escape|Delete|Backspace|Space):\s*\[/gm),
    ].map((match) => match[1].toLowerCase()),
  );
  // The two delegated sets: `KEYMAP` for the single-key path, `WALK_KEYS` for the walk. A
  // module that declares a key the handler no longer routes to it is *not* a false pass —
  // the two `assert.ok` calls above are the claim that each delegation is still wired.
  for (const match of keymap.matchAll(/^\s{2}\w+:\s*"([^"]+)"/gm)) {
    handlerKeys.add(match[1].toLowerCase());
  }
  const declaredWalk = /WALK_KEYS: readonly string\[\] = \[([^\]]*)\]/.exec(walkSource);
  assert.ok(declaredWalk, "canvas-walk.ts no longer declares WALK_KEYS, so the walk is undeclared");
  for (const name of declaredWalk[1].matchAll(/"([^"]+)"/g)) {
    handlerKeys.add(name[1].toLowerCase());
  }

  for (const row of SHORTCUT_ROWS) {
    for (const name of namesIn(row.keys)) {
      const spellings = ALIASES[name] ?? [name];
      assert.ok(
        spellings.some((spelling) => handlerKeys.has(spelling)),
        `the shortcut list advertises "${row.keys}" (${name}) and nothing in onCanvasKeyDown, ` +
          `KEYMAP or WALK_KEYS binds "${spellings[0]}"`,
      );
    }
  }
});
