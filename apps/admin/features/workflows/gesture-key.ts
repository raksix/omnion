/**
 * Every history key in the builder, and the one rule they all follow (REQ-004).
 *
 * ## The rule
 *
 * A coalesce key answers one question: **are these two changes the same gesture?** `record`
 * merges two entries when the keys match inside `COALESCE_MS`, and the merge keeps the
 * *first* `before` with the *second* `after` — so a merged pair can only be undone back to
 * before the first, and the state between the two gestures is unreachable by either key.
 *
 * That is exactly right for a drag (one gesture, many pointer frames) and exactly wrong for
 * two different actions. So the key must name **the subject**, not just the kind of action:
 * `nudge` alone says "something was nudged", and two nudges on two different cards match it.
 *
 * ## What was broken
 *
 * The keys were not written by one author with one rule. Four of the call sites already
 * named their subject — `add:${id}`, `remove:${ids}`, `edit:${id}:${fields}`,
 * `move:${ids}` (the drag, from `dragKey`) — and four named only the action: `"nudge"`,
 * `"edge-add"`, `"edge-remove"`, `"paste"`, `"auto-layout"`. The two groups disagree, and
 * `record` cannot tell which convention a given key follows.
 *
 * The nudge is the one a keyboard author hits: nudge a card right, select the next card,
 * nudge it right — two presses of the arrow key, well inside the window — and **one press of
 * undo silently reverses both**. The first card jumps back and the second jumps back, from a
 * key the author never pressed. Nothing on the screen distinguishes that from one undo
 * having been very thorough.
 *
 * Wiring a rule into a chain has the same shape and a worse answer, because the *graph* is
 * the thing that goes wrong: connect `a → b`, then 150ms later connect `b → c`, and one undo
 * takes out **both** edges, leaving an unconnected `b`. The author wired a three-node chain
 * and cannot get back to the two-node one.
 *
 * ## Why the keys live here rather than at the call sites
 *
 * Tick 50 of this REQ fixed the same class of hole one level up: a rule ("adopting a graph
 * wholesale must prune the selection") had two callers and the fix left one behind. The
 * lesson that generalises is that **a rule restated at a call site is a rule with no
 * compiler** — this is the third copy of a walk/gesture rule on this branch, after the node
 * type registry and the trigger prefix. The structural guard in `gesture-key.test.ts` reads
 * the canvas source and fails on a `commit`/`pushHistory` whose key is a bare literal, so
 * the next gesture added next year cannot quietly pick up the wrong convention.
 *
 * ## What may still share a key
 *
 * Repeated *typing into one field* (`edit:${id}:${fields}`) must coalesce, and a multi-press
 * arrow hold (`move:${ids}`) must too. That is what the subject gives us: the subject is
 * stable across the frames of one gesture and different across two.
 */

/** Join ids the way every key here does: sorted, so a gesture's key does not depend on click order. */
function subject(ids: string[]): string {
  return [...ids].sort().join(",");
}

/**
 * A node was added. The id is the new card's own, so two palette adds are two entries even
 * when the second lands on top of the first.
 */
export function addKey(nodeId: string): string {
  return `add:${nodeId}`;
}

/**
 * One or more nodes were removed. A group delete names the whole set, so deleting two cards
 * is one press and deleting one of them again is another.
 */
export function removeKey(ids: string[]): string {
  return `remove:${subject(ids)}`;
}

/**
 * A node moved — by arrow key, by nudge or by drag.
 *
 * Shared with the drag's own `dragKey`, which is the point: a drag of `[a,b]` and a nudge of
 * `[a,b]` are the same *subject*, and the drag's `endDrag` seals its group so a nudge
 * straight afterwards is still its own entry. Naming the moved set rather than the mechanism
 * is what makes holding an arrow key one press while nudging two different cards is two.
 */
export function moveKey(ids: string[]): string {
  return `move:${subject(ids)}`;
}

/**
 * A parameter was edited. The field set is part of the key so that changing one field and
 * then another in the same inspector are separate entries, while holding a key down in one
 * field coalesces into the single gesture it is.
 */
export function editKey(nodeId: string, fields: string[]): string {
  return `edit:${nodeId}:${[...fields].sort().join(",")}`;
}

/** A card was duplicated. The copy's id, because two duplicates of one card are two presses. */
export function duplicateKey(nodeId: string): string {
  return `duplicate:${nodeId}`;
}

/**
 * A group was pasted.
 *
 * The key is the *first* pasted id, not a constant, and this is the one key that cannot be
 * derived from the subject in advance — the ids are minted during the paste. Anchoring on
 * the first new id is stable for the gesture and distinct for the next one, so pasting twice
 * inside the window is two presses of undo rather than one press that empties the canvas.
 */
export function pasteKey(firstPastedId: string): string {
  return `paste:${firstPastedId}`;
}

/**
 * An edge was added or removed.
 *
 * The endpoints, not the edge id: the same port pair is the same connection, so drawing it
 * once and re-drawing it inside the window is one gesture, while `a → b` and `b → c` are two.
 * Removing the edge just added is a *different* key, which is why a connect-then-delete pair
 * stays two entries — the graph is back where it started and one undo that claims to undo
 * both would be describing a change nobody made.
 */
export function edgeKey(action: "add" | "remove", source: string, port: string, target: string): string {
  return `edge-${action}:${source}:${port}→${target}`;
}

/**
 * The canvas was auto-laid-out. The subject is every node that was placed, so a layout over a
 * three-card graph and a later layout over a five-card graph are different gestures even if
 * the second lands inside the window.
 */
export function layoutKey(movedIds: string[]): string {
  return `auto-layout:${subject(movedIds)}`;
}
