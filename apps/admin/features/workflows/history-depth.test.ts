/**
 * The builder's history is at least 50 steps deep (REQ-004, "Undo/redo restores add, move,
 * connect, delete and parameter edits at least 50 steps deep").
 *
 * ## Why this file exists at all
 *
 * The claim was resting on a constant. `HISTORY_LIMIT = 100` is a number, and a number is not a
 * claim about the product — an earlier tick removed one reason the constant was hollow (the stack
 * merged gestures that were not the same, so 50 entries were not 50 changes), and what was still
 * missing is the *walk*. Nothing pressed undo fifty times.
 *
 * ## The three ways this test is easy to get wrong
 *
 * **One kind, fifty times.** Fifty palette adds prove the stack holds fifty entries and nothing
 * about a move, a connection, a delete or a parameter edit. This REQ has already been bitten by
 * that exact substitution (a criterion about five error classes measured with one broken graph),
 * so the fifty here cycle through all five kinds in a fixed rotation.
 *
 * **Counting entries instead of walking them.** `entries.length === 50` is satisfied by a stack
 * whose entries all hold the same snapshot. What the criterion claims is that a press of undo
 * *returns the graph*, so this test replays the fifty presses and compares the reconstructed
 * graph to the one that was there — at the start, at four checkpoints in the middle, and at the
 * end. An off-by-one in the cursor passes the count and fails the walk.
 *
 * **Letting the clock separate the gestures.** Spacing fifty gestures seconds apart proves the
 * `COALESCE_MS` window opens, which is not the claim. Every gesture here lands **inside** one
 * window, so the only thing that can keep them apart is the coalesce key — which is exactly the
 * subject-named key the previous tick introduced. The last test in this file is the control: the
 * same fifty gestures carrying bare action keys merge, so a regression that reintroduces them
 * cannot pass.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  COALESCE_MS,
  HISTORY_LIMIT,
  emptyHistory,
  record,
  redo,
  redoTarget,
  undo,
  undoTarget,
  type History,
  type HistorySnapshot,
} from "./builder-history.ts";
import { addKey, edgeKey, editKey, moveKey, removeKey } from "./gesture-key.ts";
import {
  SHORTCUT_GROUPS,
  SHORTCUT_ROWS,
  historyDepthLabel,
} from "./keyboard-path.ts";

/** The depth the criterion asks for. `HISTORY_LIMIT` is what the product implements. */
const CLAIMED_DEPTH = 50;

/** How many distinct gestures are performed, and walked back. */
const GESTURES = 50;

/**
 * Every gesture is this far apart, so the whole run fits inside one coalesce window
 * (`GESTURES * GAP_MS` is asserted below against `COALESCE_MS`). Time therefore cannot be what
 * separates them.
 */
const GAP_MS = 10;

/** One card on the canvas, in the shape `snapshotOf` is asked to copy. */
interface Card {
  id: string;
  type: string;
  label: string;
  params: Record<string, unknown>;
  position: { x: number; y: number };
}

/** One connection. */
interface Link {
  id: string;
  source: string;
  source_port: string;
  target: string;
}

/**
 * The graph as the builder holds it, mutated in place by the five gesture functions below.
 *
 * The world is the *product's* state, not a fixture: each gesture snapshots it before mutating
 * and after, and the walk below is what puts those snapshots back. If a gesture mutated the
 * world without changing the snapshot, `record` would drop the entry as a no-op and the walk
 * would notice the missing press.
 */
interface World {
  cards: Card[];
  links: Link[];
}

const card = (id: string, x = 0, mode?: string): Card => ({
  id,
  type: "task",
  label: id,
  params: mode === undefined ? {} : { mode },
  position: { x, y: 0 },
});

/** What `snapshotOf` is given at the builder's call sites. */
function shoot(world: World): HistorySnapshot {
  return { nodes: world.cards.map((c) => ({ ...c, position: { ...c.position }, params: { ...c.params } })), edges: world.links.map((l) => ({ ...l })) };
}

/** A readable fingerprint, so a failed comparison says which card moved. */
function shape(snapshot: HistorySnapshot): string {
  const cards = snapshot.nodes
    .map((n) => `${(n as Card).id}@${(n as Card).position.x}${JSON.stringify((n as Card).params)}`)
    .sort();
  const links = snapshot.edges
    .map((e) => `${(e as Link).source}→${(e as Link).target}`)
    .sort();
  return `${cards.join(" ")} | ${links.join(" ")}`;
}

/**
 * A compact fingerprint, for the runs whose graphs are a hundred cards wide.
 *
 * `shape` is the right fingerprint for the fifty-gesture walk — its diffs are readable and the
 * card that moved is named. At `HISTORY_LIMIT + GESTURES` cards the same string is 3KB of
 * `x137@0{}` on both sides, and a failure that prints 6KB to say "the stack kept the oldest
 * hundred instead of the newest" is a failure nobody reads. This one answers the same question
 * with a checksum over the id, the position and the params, and keeps the two ends by name — a
 * stack that kept the wrong end still differs here, and the message says which.
 */
function digest(snapshot: HistorySnapshot): string {
  const parts = snapshot.nodes
    .map((n) => `${(n as Card).id}@${(n as Card).position.x}:${JSON.stringify((n as Card).params)}`)
    .sort();
  let hash = 0;
  for (const part of parts) {
    for (let i = 0; i < part.length; i += 1) {
      hash = (hash * 31 + part.charCodeAt(i)) | 0;
    }
  }
  const first = parts[0]?.split("@")[0] ?? "-";
  const last = parts[parts.length - 1]?.split("@")[0] ?? "-";
  return `${parts.length} cards ${first}..${last} #${hash}`;
}

/**
 * Run the five-kind rotation `GESTURES` times and return the history plus the graph states the
 * walk has to land back on.
 *
 * The rotation per cycle of five is: add a card, move it, edit one of its parameters, connect it
 * to the card before it, then delete the **oldest** live card. The delete is the one that keeps
 * this honest — if every cycle ended by removing the card it had just added, the world after
 * fifty gestures would be identical to the world before the first, and a history that undid
 * nothing at all would satisfy the walk-back. Deleting from the far end of the pool makes the
 * final graph a different graph.
 */
function performFifty(): {
  history: History;
  world: World;
  /** `checkpoints[k]` is the graph as it stood BEFORE gesture `k * 10` ran. */
  checkpoints: HistorySnapshot[];
} {
  const world: World = { cards: [card("trigger"), card("act1"), card("act2")], links: [] };
  // The order the pool is consumed from: the delete takes from the front, so the oldest card is
  // the one that goes, and the world shrinks and regrows as the cycles pass.
  const pool = ["trigger", "act1", "act2"];
  let history = emptyHistory();
  const checkpoints: HistorySnapshot[] = [];

  for (let cycle = 0; cycle < GESTURES / 5; cycle += 1) {
    const fresh = `n${cycle}`;
    const slot = cycle * 5;
    const gestures: { key: string; apply: () => void }[] = [
      { key: addKey(fresh), apply: () => { world.cards.push(card(fresh)); pool.push(fresh); } },
      {
        key: moveKey([fresh]),
        apply: () => {
          const c = world.cards.find((x) => x.id === fresh);
          assert.ok(c, `the card the move names must exist; ${fresh} is not in the world`);
          c.position = { ...c.position, x: c.position.x + 40 };
        },
      },
      {
        key: editKey(fresh, ["mode"]),
        apply: () => {
          const c = world.cards.find((x) => x.id === fresh);
          assert.ok(c, `the card the edit names must exist; ${fresh} is not in the world`);
          c.params = { ...c.params, mode: `m${cycle}` };
        },
      },
      {
        key: edgeKey("add", pool[pool.length - 2], "out", fresh),
        apply: () => {
          const source = pool[pool.length - 2];
          world.links.push({ id: `e${cycle}`, source, source_port: "out", target: fresh });
        },
      },
      {
        key: removeKey([pool[0]]),
        apply: () => {
          const victim = pool[0];
          world.links = world.links.filter((l) => l.source !== victim && l.target !== victim);
          world.cards = world.cards.filter((c) => c.id !== victim);
          pool.shift();
        },
      },
    ];

    for (const [offset, gesture] of gestures.entries()) {
      const index = slot + offset;
      // A checkpoint every ten gestures: enough to catch a cursor that drifts in the middle of
      // the run, not so many that the comparison is noise.
      if (index % 10 === 0) {
        checkpoints.push(shoot(world));
      }
      const before = shoot(world);
      gesture.apply();
      const after = shoot(world);
      assert.notEqual(shape(before), shape(after), `gesture ${index} changed nothing, so record would drop it`);
      history = record(history, { key: gesture.key, before, after, now: 1000 + index * GAP_MS });
    }
  }

  // `GESTURES` is 50 and the rotation is 5 long, so the cycles only cover the claim if the two
  // divide evenly. A fixture that silently performs 45 gestures would leave the walk short and
  // the count assertion below would be the only thing noticing.
  assert.equal(
    GESTURES % 5,
    0,
    `a rotation of five cannot cover ${GESTURES} gestures evenly; the walk would be short`,
  );
  return { history, world, checkpoints };
}

/** One press of undo, the way the builder applies it: take the target, then move the cursor. */
function pressUndo(history: History, graph: HistorySnapshot): { history: History; graph: HistorySnapshot } {
  const target = undoTarget(history);
  assert.ok(target, "a press of undo with entries left must name a target");
  return { history: undo(history), graph: target };
}

test("the implemented depth covers the depth the criterion claims", () => {
  // The screen says a number (see `historyDepthLabel`); this is what makes that number a claim
  // rather than a string. If the limit is ever lowered, this fails before the label lies.
  assert.ok(
    HISTORY_LIMIT >= CLAIMED_DEPTH,
    `the history keeps ${HISTORY_LIMIT} entries, the criterion claims ${CLAIMED_DEPTH}`,
  );
  assert.match(historyDepthLabel(), /\d+/, "the toolbar's undo label must state a depth");
  assert.ok(
    historyDepthLabel().includes(String(HISTORY_LIMIT)),
    "the label must report the limit the history actually implements, not a number of its own",
  );
});

test("fifty distinct gestures across all five kinds are fifty presses, not fifty entries of one kind", () => {
  const { history } = performFifty();
  assert.equal(history.entries.length, GESTURES, "every gesture must be its own entry");
  // The five kinds are all present, and the claim is about the mix rather than the total. A count
  // of fifty adds would pass the assertion above and say nothing the criterion asks.
  for (const prefix of ["add:", "move:", "edit:", "edge-add:", "remove:"]) {
    assert.ok(
      history.entries.some((e) => e.key.startsWith(prefix)),
      `the fifty gestures must include a ${prefix} entry`,
    );
  }
});

test("walking fifty undos back returns the graph the first gesture started from", () => {
  const { history, world, checkpoints } = performFifty();
  const start = shoot({ cards: [card("trigger"), card("act1"), card("act2")], links: [] });

  // The world after the run must NOT equal the world before it, or the walk below would pass
  // against a history that did nothing. Asserted before the walk, because this is the premise
  // the whole file rests on.
  assert.notEqual(
    shape(shoot(world)),
    shape(start),
    "fifty gestures that end where they began cannot show fifty undoable steps",
  );

  let current = history;
  let graph = shoot(world);
  for (let press = 0; press < GESTURES; press += 1) {
    const next = pressUndo(current, graph);
    current = next.history;
    graph = next.graph;

    // `checkpoints[k]` was taken BEFORE gesture `k * 10` ran, so it is the graph that exists
    // after exactly `GESTURES - k * 10` presses of undo. Getting this off by one compares the
    // walk against the state one gesture late and reports a cursor defect that is not there, so
    // the press count is derived from the checkpoint rather than the other way round.
    for (const [k, expected] of checkpoints.entries()) {
      if (GESTURES - k * 10 === press + 1) {
        assert.equal(
          shape(graph),
          shape(expected),
          `after ${press + 1} presses the graph must be the one that was there before gesture ${k * 10}`,
        );
      }
    }
  }

  // The last checkpoint is the opening graph, reached by the final press.
  assert.equal(
    shape(graph),
    shape(start),
    "fifty presses of undo must land on the graph the first gesture started from",
  );
  assert.equal(
    graph.nodes.length,
    start.nodes.length,
    "fifty presses of undo must land on three cards, not on whatever the stack held",
  );
  assert.equal(
    current.cursor,
    -1,
    "the cursor must be past the beginning, which is what the undo button reads to disable itself",
  );
});

test("the fifty gestures sit inside one coalesce window, so the clock is not what separates them", () => {
  assert.ok(
    GESTURES * GAP_MS < COALESCE_MS,
    `the whole run spans ${GESTURES * GAP_MS}ms and the window is ${COALESCE_MS}ms; if the run no longer fits inside one window this file is no longer testing the key`,
  );

  // The control. The old convention was a bare *action* key — `"nudge"`, `"edge-add"` — and the
  // defect was not that the key was short, it was that one key covered *many subjects*. So the
  // control has to use ONE key for every gesture, not one per kind: five rotating names never
  // match each other, nothing merges, and the control passes on a product that is broken. This
  // is the tick-48 mistake again in a new place — a control that cannot fail is a rubber stamp.
  let merged = emptyHistory();
  const world: World = { cards: [], links: [] };
  for (let i = 0; i < GESTURES; i += 1) {
    const before = shoot(world);
    world.cards.push(card(`x${i}`));
    merged = record(merged, { key: "nudge", before, after: shoot(world), now: 1000 + i * GAP_MS });
  }
  assert.equal(
    merged.entries.length,
    1,
    "one action key for fifty gestures is ONE entry, which is the defect the subject-named keys fixed",
  );
  // And the walk that the real keys support is the walk that control cannot do.
  assert.equal(undoTarget(merged)?.nodes.length, 0, "one press of undo under a bare key empties the canvas");
});

test("the fifty presses still work after the history has overflowed its limit", () => {
  // The path the whole file missed, and the one the criterion actually names.
  //
  // Fifty gestures fit inside `HISTORY_LIMIT = 100`, so the run above never reaches the trim —
  // and the trim is the one line in `record` that decides *which* fifty survive when the stack
  // is full. Keeping the oldest instead of the newest passes every test in the suite (this one
  // was measured doing exactly that: 278 green with `entries.slice(0, LIMIT)` in place of
  // `entries.slice(entries.length - LIMIT)`), because a history that discards the *recent*
  // entries still holds the right *number* of them and every one of those undoes without
  // complaint. The author loses the last fifty edits and the oldest ones stay resurrectable,
  // which is the failure a depth claim exists to rule out — the stack is 100 deep and useless.
  //
  // So this walks HISTORY_LIMIT gestures, then fifty more, and then presses undo fifty times. The
  // fifty it returns must be the fifty that were *just* made, not the fifty that preceded them.
  let history = emptyHistory();
  const world: World = { cards: [], links: [] };
  // The canvas before every gesture, so the walk can be compared against the state that was
  // actually on screen rather than against a count. `worldBefore[i]` is the world before
  // gesture `i` ran, so the (i+1)-th undo must restore `worldBefore[i]`.
  const worldBefore: HistorySnapshot[] = [];
  for (let i = 0; i < HISTORY_LIMIT + GESTURES; i += 1) {
    const before = shoot(world);
    worldBefore.push(before);
    world.cards.push(card(`x${i}`));
    history = record(history, { key: addKey(`x${i}`), before, after: shoot(world), now: 1000 + i });
  }
  assert.equal(
    history.entries.length,
    HISTORY_LIMIT,
    "the history is bounded no matter how many gestures are made",
  );

  // Walk back the fifty that survived the trim, one press at a time, and check each press
  // against the graph that was on screen when that gesture was made. Comparing the card *count*
  // alone is what let the broken trim pass: a stack holding the OLDEST hundred also has the
  // right count, and its first undo also lands on a graph one card shorter — just the wrong one.
  // The identity of the surviving entries is the assertion; the walk is what makes it a claim
  // about presses rather than about an array.
  //
  // After 150 gestures the stack holds the newest hundred — gestures 50..149 — so the oldest
  // surviving entry is the one for `x50`. A trim that kept the oldest hundred would hold `x0`,
  // which is the whole defect in one character.
  assert.equal(
    history.entries[0].key,
    addKey(`x${HISTORY_LIMIT / 2}`),
    "the trim must keep the newest entries, or the last edits are the ones an author cannot undo",
  );

  let current = history;
  let graph = shoot(world);
  // The stack holds entries for gestures `HISTORY_LIMIT/2 … 149`, and `entries[99]` — the one the
  // cursor is on — is the gesture for `x149`, whose `before` is the canvas holding 149 cards.
  // Each press walks one gesture back, so press k restores the world before gesture `149 - k`.
  // Getting this off by one is the same mistake the fifty-gesture walk made first time, and the
  // assertion below is deliberately written as the *indexed* read rather than a loop counter so
  // the two cannot be derived from each other.
  for (let press = 0; press < GESTURES; press += 1) {
    const undone = pressUndo(current, graph);
    current = undone.history;
    graph = undone.graph;
    assert.equal(
      digest(graph),
      digest(worldBefore[HISTORY_LIMIT + GESTURES - 1 - press]),
      `press ${press + 1} past the limit must undo the gesture that was made, not resurrect an older one`,
    );
  }
  // Fifty presses past the trim land on the graph that was on screen when gesture 50 was made:
  // the hundred cards `x0..x99`, which is the deepest state an author can still get back to.
  assert.equal(
    graph.nodes.length,
    HISTORY_LIMIT,
    "and it must stop at the oldest surviving entry rather than at the beginning of the session",
  );
  assert.equal(
    (graph.nodes[0] as Card).id,
    "x0",
    "the graph the author can no longer undo past still contains the oldest card it kept",
  );
  assert.equal(
    current.cursor,
    HISTORY_LIMIT / 2 - 1,
    "the cursor stops where the history starts, which is what the undo button reads to disable itself",
  );
});

test("redo walks the same fifty back up", () => {
  // The criterion names undo/redo as one thing, and redo is the half that re-applies `after`
  // rather than restoring `before` — a different array, so a defect in one is not a defect in
  // the other.
  //
  // This drives `redoTarget`, the way the canvas does. The first draft read
  // `entries[cursor + 1].after` directly and was green with `redoTarget` itself off by one —
  // the mutation that made redo re-apply the step that had just been undone passed the whole
  // file. A test that reaches past the function under test is testing its caller, and on this
  // REQ the caller is the thing that is not written yet.
  const { history, world } = performFifty();
  const top = shoot(world);

  let current = history;
  let graph = shoot(world);
  for (let press = 0; press < GESTURES; press += 1) {
    current = pressUndo(current, graph).history;
  }
  const bottom = graph;
  // Captured *before* the redo presses: after them the cursor is back on the newest entry and
  // there is nothing left to redo, which is a true statement about the wrong moment.
  const atBottom = current;
  for (let press = 0; press < GESTURES; press += 1) {
    const target = redoTarget(current);
    assert.ok(target, `redo press ${press + 1} must name a target`);
    graph = target;
    current = redo(current);
  }
  assert.equal(shape(graph), shape(top), "fifty presses of redo must return the graph the run ended on");
  assert.equal(
    current.cursor,
    GESTURES - 1,
    "and leave the cursor on the newest entry",
  );
  // The control for the whole walk: the first redo press must not return the state redo just
  // left. Without it, a `redoTarget` that always answered "no change" would walk fifty presses
  // to the same graph the run already ended on and pass — the cursor check alone cannot see it,
  // because such a history still ends on the newest entry.
  const firstRedo = redoTarget(atBottom);
  assert.ok(firstRedo, "the first redo press after fifty undos must name a target");
  assert.notEqual(
    shape(firstRedo),
    shape(bottom),
    "one redo press must not return the state redo just left",
  );
});

test("the undo row's depth is the row, not a copy of it", () => {
  // `historyDepthLabel` exists so the sheet cannot drift from the constant, and a function that
  // nothing calls has drifted already. Hard-coding the row again passes every behavioural test
  // in this file — the depth is still real, only the sentence about it is a lie waiting to
  // happen the day the limit is lowered. So the row is read out of the catalogue itself.
  const row = SHORTCUT_GROUPS.find((g) => g.title === "History and clipboard")?.rows.find(
    (r) => r.keys === "⌘Z",
  );
  assert.ok(row, "the undo row must exist in the keyboard sheet");
  assert.equal(
    row.label,
    historyDepthLabel(),
    "the sheet's undo row must carry the derived label, so lowering HISTORY_LIMIT moves both",
  );
  assert.ok(
    SHORTCUT_ROWS.some((r) => r.label === historyDepthLabel()),
    "and the flattened catalogue the panel renders must contain it too",
  );
});
