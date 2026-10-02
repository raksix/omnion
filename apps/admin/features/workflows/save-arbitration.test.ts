/**
 * The save-arbitration rules (REQ-004, slice 2).
 *
 * The criterion — "⌘S during a pending autosave does not write twice" — is really two
 * claims, and they are about different moments, so they are tested as two claims:
 *
 *   1. A press while the debounce is armed produces **one** write, not two. The failure
 *      this catches is the obvious implementation: call `persist` and leave the timer
 *      running, and the graph is written once by ⌘S and again 1.2s later. The version moves
 *      twice, and a second tab is handed a conflict no author caused.
 *   2. A press while a write is on the wire produces **no new** write. The failure here is
 *      subtler and is the one that loses data: the request has already left quoting
 *      `graph_version = N`, so a second one also quotes N. Whichever loses the race in the
 *      database is refused as a conflict — a conflict the author manufactured by pressing
 *      the key that was supposed to help.
 */
import { test } from "node:test";
import assert from "node:assert/strict";

import {
  arbitrateSave,
  savePressIsAcknowledged,
  type SaveAction,
} from "./save-arbitration.ts";

test("a press with a debounce armed writes now, and the debounce is the thing that gets cancelled", () => {
  // One action, and it is the one that implies cancelling the timer. If this ever became
  // "write-now-and-also-let-the-timer-fire", the criterion is false again.
  const action = arbitrateSave({ debounceArmed: true, writeInFlight: false });
  assert.equal(action, "write-now");
});

test("a press with nothing pending still saves — the key is never a no-op on a clean graph", () => {
  // ⌘S on an unchanged graph is a legitimate thing to press (the author wants certainty,
  // not a change), and refusing it would make the indicator lie in the other direction.
  assert.equal(arbitrateSave({ debounceArmed: false, writeInFlight: false }), "write-now");
});

test("a press while a write is on the wire starts nothing — this is the case that loses data", () => {
  const action = arbitrateSave({ debounceArmed: false, writeInFlight: true });
  assert.equal(
    action,
    "join-in-flight",
    "a second write would quote the same graph_version as the first and race it",
  );
});

test("in-flight wins over an armed debounce: the request that has left is the one that can collide", () => {
  // Both conditions true is the exact moment the criterion names. The order matters and is
  // the reason this is a function at all: checking the debounce first would answer
  // "write-now" here, cancel the timer, and start a second write against a version the
  // first is about to replace.
  const action = arbitrateSave({ debounceArmed: true, writeInFlight: true });
  assert.equal(action, "join-in-flight");
});

test("every press is acknowledged, so the indicator never sits still for a key that was answered", () => {
  // The regression this guards is subtle but real: if `join-in-flight` left the indicator
  // alone, the press would be invisible, and an author who cannot tell whether ⌘S landed
  // presses it again. The rule that prevents a second write would have taught the opposite
  // habit, and the double write would come back through the front door.
  const actions: SaveAction[] = ["write-now", "join-in-flight"];
  for (const action of actions) {
    assert.equal(savePressIsAcknowledged(action), true, `${action} must be acknowledged`);
  }
});
