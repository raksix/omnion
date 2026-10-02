/**
 * Two different gestures must never share one press of undo (REQ-004).
 *
 * ## The defect this file is about
 *
 * `record` merges two entries when their coalesce **keys** match inside `COALESCE_MS`, and the
 * merge keeps the *first* `before` with the *second* `after`. That is right for a drag, which
 * fires one change per pointer frame, and wrong for two different actions. So the key has to
 * answer "are these the same gesture?", and the only thing that answers it is **the subject**:
 * which cards, which connection, which field.
 *
 * Four call sites already named their subject (`add:${id}`, `remove:${ids}`,
 * `edit:${id}:${fields}`, `move:${ids}`) and four named only the action — `"nudge"`,
 * `"edge-add"`, `"edge-remove"`, `"paste"`, `"auto-layout"`. Both groups were green: the
 * history module's tests only ever used keys that were already subject-bearing, so a key that
 * merged unrelated gestures was indistinguishable from a key that did not.
 *
 * The reachability is the point. Nudge a card right, select the next card, nudge it right —
 * two presses of the arrow key, well inside the 600ms window — and one press of undo silently
 * reverses **both**. No second tab, no conflict, no pointer.
 *
 * ## Why these are written against the keys and not against the history
 *
 * A test that builds its own keys proves the *history module* honours matching keys, which it
 * always did and which was never the question. The question is **which keys the product emits**,
 * so the behaviour cases below use the very functions the canvas calls, and the structural guard
 * reads the canvas source and fails on any `commit`/`pushHistory` handed a bare string literal.
 * That guard is the arm the shortcut catalogue has and this one did not: a key can be bound
 * anywhere, and only a text check catches a gesture added next year.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { COALESCE_MS, emptyHistory, record, sealGroup, undoTarget, type History, type HistorySnapshot } from "./builder-history.ts";
import { dragKey } from "./drag-history.ts";
import {
  addKey,
  duplicateKey,
  editKey,
  edgeKey,
  layoutKey,
  moveKey,
  pasteKey,
  removeKey,
} from "./gesture-key.ts";

const BUILDER_SOURCE = readFileSync(new URL("./builder-view.tsx", import.meta.url), "utf8");

interface TestEdge {
  id: string;
  source: string;
  source_port: string;
  target: string;
}

/** A graph keyed by id → x, so an assertion can read a position without a node factory. */
function snap(positions: Record<string, number>, edges: TestEdge[] = []): HistorySnapshot {
  return {
    nodes: Object.entries(positions).map(([id, x]) => ({
      id,
      type: "task",
      label: id,
      params: {},
      position: { x, y: 0 },
    })),
    edges,
  };
}

const xOf = (s: HistorySnapshot, id: string): number | undefined =>
  (s.nodes.find((n) => (n as { id: string }).id === id) as { position: { x: number } } | undefined)?.position.x;

const edge = (id: string, source: string, target: string): TestEdge => ({
  id,
  source,
  source_port: "out",
  target,
});

/** Two gestures, `gap` ms apart — inside the window unless a test says otherwise. */
function twoGestures(
  first: { key: string; before: HistorySnapshot; after: HistorySnapshot },
  second: { key: string; before: HistorySnapshot; after: HistorySnapshot },
  gap = 150,
): History {
  let history = record(emptyHistory(), { ...first, now: 1000 });
  history = record(history, { ...second, now: 1000 + gap });
  return history;
}

// ---------------------------------------------------------------------------------------------
// The two keyboard-reachable defects.
// ---------------------------------------------------------------------------------------------

test("nudging two different cards is two presses of undo, not one", () => {
  // The author: select card `a`, press Right, select card `b`, press Right. Both inside 150ms.
  const history = twoGestures(
    { key: moveKey(["a"]), before: snap({ a: 0, b: 0 }), after: snap({ a: 8, b: 0 }) },
    { key: moveKey(["b"]), before: snap({ a: 8, b: 0 }), after: snap({ a: 8, b: 8 }) },
  );

  assert.equal(
    history.entries.length,
    2,
    "two cards nudged by the keyboard are two gestures, and the window is 600ms wide",
  );
  const afterFirst = undoTarget(history);
  assert.ok(afterFirst, "the second gesture must be undoable");
  assert.equal(
    xOf(afterFirst, "a"),
    8,
    "one undo must leave the FIRST nudge standing — the second card goes back, the first stays",
  );
  assert.equal(xOf(afterFirst, "b"), 0, "only the card the author last nudged moves back");
});

test("a key that names the action instead of the subject merges two gestures (the mutation)", () => {
  // This is the shape the product had: `"nudge"` for both. It is asserted as a fact about a
  // *string*, because the product's defect was precisely that the key could not tell these
  // apart — so a test that only ever used real keys could never have caught it.
  const history = twoGestures(
    { key: "nudge", before: snap({ a: 0, b: 0 }), after: snap({ a: 8, b: 0 }) },
    { key: "nudge", before: snap({ a: 8, b: 0 }), after: snap({ a: 8, b: 8 }) },
  );
  assert.equal(history.entries.length, 1, "the old key merged them; this is the claim under test");
  const target = undoTarget(history);
  assert.ok(target);
  assert.equal(xOf(target, "a"), 0, "one press put BOTH cards back — from a key never pressed");
  assert.equal(xOf(target, "b"), 0);
});

test("wiring a chain one connection at a time keeps each connection separately undoable", () => {
  // The graph-level version, and the worse answer: connect a→b, then 150ms later connect b→c.
  // One merged entry means one undo removes BOTH edges and leaves a `b` nothing reaches.
  //
  // The first gesture adds `a→b` (its `before` and `after` differ — a no-op would be dropped by
  // `record` and the test would silently measure a single entry for the wrong reason).
  const history = twoGestures(
    { key: edgeKey("add", "a", "out", "b"), before: snap({ a: 0, b: 0, c: 0 }), after: snap({ a: 0, b: 0, c: 0 }, [edge("e1", "a", "b")]) },
    { key: edgeKey("add", "b", "out", "c"), before: snap({ a: 0, b: 0, c: 0 }, [edge("e1", "a", "b")]), after: snap({ a: 0, b: 0, c: 0 }, [edge("e1", "a", "b"), edge("e2", "b", "c")]) },
  );

  assert.equal(history.entries.length, 2, "a two-edge chain is built one connection at a time");
  const target = undoTarget(history);
  assert.ok(target);
  assert.equal(
    target.edges.length,
    1,
    "one undo must leave the FIRST connection in place",
  );
  assert.equal((target.edges[0] as TestEdge).id, "e1");
});

// ---------------------------------------------------------------------------------------------
// What MUST still coalesce. Without these, the fix is "never merge anything" and every drag
// costs one undo per pointer frame — the opposite defect, and one the criteria also forbid.
// ---------------------------------------------------------------------------------------------

test("holding the arrow key on one card is still ONE press, not one per keypress", () => {
  // The reason the key carries the subject: the subject is stable across the frames of a
  // gesture. Ten presses of Right on the same card, 50ms apart, are one intent.
  let history = emptyHistory();
  for (let i = 1; i <= 10; i += 1) {
    history = record(history, {
      key: moveKey(["a"]),
      before: snap({ a: (i - 1) * 8 }),
      after: snap({ a: i * 8 }),
      now: 1000 + i * 50,
    });
  }
  assert.equal(history.entries.length, 1, `one held key is one gesture, got ${history.entries.length}`);
  const target = undoTarget(history);
  assert.ok(target);
  assert.equal(xOf(target, "a"), 0, "and it returns the card to where the gesture started");
});

test("a drag and a nudge of the same set are one subject, so the drag's seal is what separates them", () => {
  assert.equal(dragKey(["a", "b"]), moveKey(["a", "b"]), "one definition of a move, not two");
  // The gesture boundary is the drag's `endDrag` → `sealGroup`, not the key. A nudge straight
  // after a released drag of the same cards would otherwise extend the drag's entry, and the
  // position between the two would be unreachable — the hole `sealGroup` was written for.
  let history = record(emptyHistory(), {
    key: dragKey(["a"]),
    before: snap({ a: 0 }),
    after: snap({ a: 40 }),
    now: 1000,
  });
  history = record(history, { key: moveKey(["a"]), before: snap({ a: 40 }), after: snap({ a: 48 }), now: 1030 });
  assert.equal(history.entries.length, 1, "without a seal the nudge extends the drag");

  // Now the sealed case, which is what `endDrag` actually does.
  let sealed = emptyHistory();
  sealed = record(sealed, { key: dragKey(["a"]), before: snap({ a: 0 }), after: snap({ a: 40 }), now: 1000 });
  sealed = sealGroup(sealed, 1030);
  sealed = record(sealed, { key: moveKey(["a"]), before: snap({ a: 40 }), after: snap({ a: 48 }), now: 1060 });
  assert.equal(sealed.entries.length, 2, "a released drag and a later nudge are two gestures");
});

test("re-typing one field coalesces, but editing two fields in one visit does not", () => {
  const typed = twoGestures(
    { key: editKey("a", ["label"]), before: snap({ a: 0 }), after: snap({ a: 1 }) },
    { key: editKey("a", ["label"]), before: snap({ a: 1 }), after: snap({ a: 2 }) },
  );
  assert.equal(typed.entries.length, 1, "holding a key in one field is one gesture");

  const twoFields = twoGestures(
    { key: editKey("a", ["label"]), before: snap({ a: 0 }), after: snap({ a: 1 }) },
    { key: editKey("a", ["url"]), before: snap({ a: 1 }), after: snap({ a: 2 }) },
  );
  assert.equal(twoFields.entries.length, 2, "two fields in one visit are two intentions");
});

test("connecting then deleting the same edge stays two entries", () => {
  // The graph is back where it started, so one undo claiming to reverse both would describe a
  // change nobody made. This is the case the OLD constant keys got right by accident
  // (`edge-add` ≠ `edge-remove`) and the new ones must not lose.
  const history = twoGestures(
    { key: edgeKey("add", "a", "out", "b"), before: snap({ a: 0, b: 0 }), after: snap({ a: 0, b: 0 }, [edge("e1", "a", "b")]) },
    { key: edgeKey("remove", "a", "out", "b"), before: snap({ a: 0, b: 0 }, [edge("e1", "a", "b")]), after: snap({ a: 0, b: 0 }) },
  );
  assert.equal(history.entries.length, 2, "add and remove of one connection are different gestures");
});

test("two pastes inside the window are two presses, not one press that empties the canvas", () => {
  const history = twoGestures(
    { key: pasteKey("p1"), before: snap({ a: 0 }), after: snap({ a: 0, p1: 40 }) },
    { key: pasteKey("p2"), before: snap({ a: 0, p1: 40 }), after: snap({ a: 0, p1: 40, p2: 40 }) },
  );
  assert.equal(history.entries.length, 2);
  const target = undoTarget(history);
  assert.ok(target);
  assert.equal(target.nodes.length, 2, "one undo takes the second paste only");
});

test("a group nudge is one gesture, and one card is not a group", () => {
  const group = twoGestures(
    { key: moveKey(["a", "b"]), before: snap({ a: 0, b: 0 }), after: snap({ a: 8, b: 8 }) },
    { key: moveKey(["a", "b"]), before: snap({ a: 8, b: 8 }), after: snap({ a: 16, b: 16 }) },
  );
  assert.equal(group.entries.length, 1, "a marquee-then-arrow move is one gesture");
  assert.notEqual(moveKey(["a"]), moveKey(["a", "b"]), "one card is not a group");
  assert.equal(moveKey(["b", "a"]), moveKey(["a", "b"]), "click order must not change the key");
});

test("two layouts of differently-sized graphs are different gestures", () => {
  const history = twoGestures(
    { key: layoutKey(["a", "b"]), before: snap({ a: 0, b: 0 }), after: snap({ a: 40, b: 40 }) },
    { key: layoutKey(["a", "b", "c"]), before: snap({ a: 40, b: 40 }), after: snap({ a: 40, b: 40, c: 40 }) },
  );
  assert.equal(history.entries.length, 2, "a layout over a different set of cards is its own entry");
});

test("removing two different groups is two presses", () => {
  const history = twoGestures(
    { key: removeKey(["a"]), before: snap({ a: 0, b: 0 }), after: snap({ b: 0 }) },
    { key: removeKey(["b"]), before: snap({ b: 0 }), after: snap({}) },
  );
  assert.equal(history.entries.length, 2, "one Del per card, even inside the window");
  assert.equal(removeKey(["a"]), removeKey(["a"]), "a stable key for the same gesture");
  assert.notEqual(removeKey(["a"]), addKey("a"), "a remove never shares a key with an add");
  assert.notEqual(duplicateKey("a"), addKey("a"), "nor a duplicate with an add");
});

test("a change past the window is a new entry even with the same key", () => {
  // The control for the whole file: the fix must not have broken the window itself.
  const history = twoGestures(
    { key: moveKey(["a"]), before: snap({ a: 0 }), after: snap({ a: 8 }) },
    { key: moveKey(["a"]), before: snap({ a: 8 }), after: snap({ a: 16 }) },
    COALESCE_MS + 10,
  );
  assert.equal(history.entries.length, 2, "the window still ends a gesture");
});

// ---------------------------------------------------------------------------------------------
// The structural guard — the arm this rule did not have.
// ---------------------------------------------------------------------------------------------

test("no gesture hands the history a bare string key", () => {
  // The point of the guard: `record` is correct and always was; the defect lived in which key
  // a call site chose. A behavioural test cannot see a call site that does not exist yet, so
  // the convention needs a compiler — which here is a text check, and is honestly limited to
  // that (it cannot verify a key function *computes* the right subject, only that it is called).
  //
  // A gesture added next year with `commit("do-something", …)` fails here. That is the whole
  // value: the mistake this file is about was made eight times by four authors in one file, and
  // nothing about it was visible to a test.
  // Comments are stripped first, and that is not a nicety: two doc comments still describe the
  // OLD keys (`it routes through \`commit("nudge", …)\``), so a guard that reads raw text reports
  // a defect in a sentence rather than in a call. A guard that cries wolf on prose gets deleted,
  // and this one is the only thing standing between the next gesture and the same bug. Verified
  // safe on this file: no `//` appears inside a string literal, which is the case where a naive
  // strip would eat real code — re-checked here rather than assumed, since a file that later
  // grows a URL string would otherwise silently lose half its call sites.
  assert.doesNotMatch(
    BUILDER_SOURCE,
    /"[^"\n]*\/\/[^"\n]*"/,
    "a // inside a string literal would make the comment strip eat real code; strip comments more carefully",
  );
  const code = BUILDER_SOURCE
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .replace(/^\s*\/\/.*$/gm, " ");

  const CALLS = /\b(?:commit|pushHistory)\(\s*(?:"([^"]*)"|'([^']*)'|`([^`]*)`)/g;
  const bare: string[] = [];
  for (const match of code.matchAll(CALLS)) {
    const literal = match[1] ?? match[2] ?? match[3] ?? "";
    if (literal !== "") {
      bare.push(literal);
    }
  }
  assert.deepEqual(
    bare,
    [],
    `every history key must name its subject; bare action keys found: ${JSON.stringify(bare)}`,
  );
});

test("the canvas actually calls the key functions the guard is pretending to police", () => {
  // A guard that passes because the call sites were deleted is a rubber stamp, and this REQ
  // has been bitten by that shape twice (`pageIsAlive` defined and never called; the reload
  // rebase pruning one caller of two). So the guard is paired with a count: each key function
  // must be *imported and called*, so removing a gesture cannot silently satisfy the rule above.
  for (const [name, call] of [
    ["addKey", /addKey\(/],
    ["removeKey", /removeKey\(/],
    ["moveKey", /moveKey\(/],
    ["editKey", /editKey\(/],
    ["duplicateKey", /duplicateKey\(/],
    ["pasteKey", /pasteKey\(/],
    ["edgeKey", /edgeKey\(/],
    ["layoutKey", /layoutKey\(/],
  ] as const) {
    assert.match(BUILDER_SOURCE, call, `${name} must be called from the canvas, not merely defined`);
  }
  // The drag delegates rather than restating, so the move has exactly one definition.
  const dragSource = readFileSync(new URL("./drag-history.ts", import.meta.url), "utf8");
  assert.match(dragSource, /return moveKey\(ids\);/, "dragKey must delegate to moveKey, not restate it");
  assert.doesNotMatch(
    dragSource,
    /`move:\$\{/,
    "a second copy of the move key is a rule with no compiler",
  );
});
