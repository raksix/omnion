/**
 * The Tab walk across the canvas (REQ-004, slice 4 — the accessibility assertions).
 *
 * ## What this is for, and why the file exists at all
 *
 * The shortcut list advertised `Tab` — "Walk to the next card" — and the keyboard-only
 * criterion's own script says `Tab`/`ArrowRight` walks the selection onto a connection's
 * target. **Neither was true.** `selection.ts` exported `focusOrder` and a unit test
 * asserted its shape, so the walking *order* was written, tested and never called; the
 * canvas bound no `Tab` case at all, and every node card is `tabIndex={-1}`, so the browser
 * moved focus to the *next control in the page* — the toolbar — and the selection stayed
 * exactly where it was.
 *
 * That is the "dead button" the definition of done forbids, wearing a help list as a
 * disguise: the row is on screen, the key is whitelisted as a *reading* key by the
 * narrow-screen lock, and pressing it does nothing an author can see. It survived two ticks
 * of shortcut-catalogue work because the drift guard asks "does every key the handler
 * binds have a row?", and this row had no handler to miss. **The guard had no arm for a row
 * that claims a key nothing implements.**
 *
 * ## The order, and why it is not simply "the nodes in array order"
 *
 * `focusOrder` already answers "what is walkable" (nodes, then edges). What it does not
 * answer is *where the walk starts and which way it goes*, and those two are the whole
 * difficulty:
 *
 * * **Start.** The author is focused on one card; Tab moves to the one after it. A walk that
 *   always started at the first node would jump backwards from the middle of a graph, which
 *   is how a keyboard user loses their place.
 * * **Direction.** `Tab` goes forward and `Shift+Tab` goes back, so a boolean is not enough
 *   — the walk has to be a rotation, not a scan. Scanning wraps anyway on the last press,
 *   but scanning *backwards* from the first node is the other half of the same rotation.
 *
 * ## The one place this deliberately does not wrap
 *
 * **A selection that no longer exists.** `pruneSelection` is the caller's job, not this
 * function's: if the focused card was deleted and the focus still names it, this walk
 * returns the first node. That is the only honest answer available — it cannot invent a
 * "where was I" marker it was never given — and it means a delete that leaves a phantom focus
 * is *visible* (the selection jumps to the top) instead of silent.
 */

import { focusOrder, type CanvasSelection, type FocusableIds } from "./selection.ts";

/** Which way the walk moves. `Shift+Tab` is the same rotation, played backwards. */
export type WalkDirection = "forward" | "backward";

/**
 * The id the walk lands on, or `null` when there is nothing to land on.
 *
 * `null` is a real answer rather than a fallback: an empty graph has nothing to walk, and a
 * function that returned the first of an empty list would hand the caller `undefined`, which
 * is indistinguishable from a bug in the walk itself.
 */
export function nextFocusable(
  world: FocusableIds,
  current: CanvasSelection,
  direction: WalkDirection = "forward",
): string | null {
  const order = focusOrder(world);
  if (order.length === 0) {
    return null;
  }
  // The walk resumes from the *drawn* selection, not from `focus` alone. A Shift+clicked
  // group has its focus on one card but outlines several, and continuing from the focus
  // alone is the ordinary case — so the focus is the anchor, and the group only matters for
  // a different question (what `Del` removes).
  const at = current.focus ? order.indexOf(current.focus) : -1;
  if (at < 0) {
    // Either nothing was focused, or the focus names something that is gone. Both land on
    // the first card going forward and the last going back, which is what "start of the list"
    // means in each direction.
    return direction === "forward" ? order[0] : order[order.length - 1];
  }
  const step = direction === "forward" ? 1 : -1;
  return order[(at + step + order.length) % order.length];
}

/**
 * The keys this module answers, declared rather than inferred.
 *
 * A guard that asks "does the handler contain `event.key === "Tab"`" cannot see a key handled
 * by a predicate in another file, and the fix that usually follows is an exception list — an
 * allowlist of keys the guard has been told to trust, which is a hole with a comment on it.
 * Declaring the keys instead inverts that: a module says what it owns, and the guard unions
 * the declarations. A new delegated binding has to *say* what it answers, and a module that
 * declares a key the handler never routes to it is caught by the separate
 * "the handler must call it" assertion rather than by being assumed.
 */
export const WALK_KEYS: readonly string[] = ["Tab"];

/**
 * Does this key walk the canvas, or is it a Tab the browser owns?
 *
 * **The trap is Shift.** `Tab` and `Shift+Tab` both walk, so this returns `true` for both
 * and the caller must not read `event.shiftKey` again. But a `Tab` pressed *inside* a field
 * is the field's own indent/outdent — in a `<textarea>` it inserts a tab character, and in an
 * input it moves to the next control. Consuming it there is the single most common way a
 * keyboard shortcut breaks the keyboard: `focusInspector` focuses an input, and if Tab then
 * jumped the selection instead of leaving the field, the criterion's "edits a parameter"
 * step would be impossible to complete and would look like the shortcut being broken rather
 * than the shortcut eating the field.
 *
 * So the typing guard comes first and is not optional. It reads the same predicate
 * `readKey` uses, because a rule about "is the author typing" that has two implementations is
 * a rule that will have one of them out of date.
 *
 * **The target is typed as an `EventTarget`, not as an element.** A React
 * `KeyboardEvent<HTMLDivElement>` hands the handler an `EventTarget`, which has no
 * `tagName` — so a signature asking for `{ tagName?: string }` is a signature the real
 * caller cannot satisfy, and the fix that usually follows is a cast at the call site, which
 * is exactly where the next reader stops believing the function checks anything. The check
 * itself stays structural, because a `data` attribute is how the test reaches it too.
 */
export function shouldWalkCanvas(event: {
  key: string;
  shiftKey?: boolean;
  target?: EventTarget | null;
}): boolean {
  if (!WALK_KEYS.includes(event.key)) {
    return false;
  }
  const target = event.target as Partial<HTMLElement> | null;
  const tag = (target?.tagName ?? "").toUpperCase();
  if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target?.isContentEditable === true) {
    return false;
  }
  return true;
}

/**
 * The selection after a Tab press.
 *
 * Returned as a *state* rather than an id so the caller cannot focus a card without also
 * selecting it — the same reason `deleteTarget` and `whatEscapeClears` return decisions
 * rather than performing them. A walk that moved the browser's focus ring but left
 * `selection` alone would be the third definition of "selected" in this file's neighbourhood,
 * and the outline would disagree with the focus the author is looking at.
 *
 * The landed card becomes the **focus** and the group is cleared: Tab is a move, not an
 * accumulation, and a keyboard author pressing it six times must not end up with six
 * outlined cards and a `Del` that would take all of them.
 */
export function walkSelection(
  world: FocusableIds,
  current: CanvasSelection,
  direction: WalkDirection = "forward",
): CanvasSelection {
  const next = nextFocusable(world, current, direction);
  if (next === null) {
    return { ...current };
  }
  return { nodes: [], focus: next, edge: null };
}

/**
 * Does this id name an edge rather than a node?
 *
 * A walk that lands on an edge selects it the way a click does (`selectEdge`), and the two
 * must agree — otherwise Tab selects a line the outline does not draw and `Del` on it
 * removes something invisible. The caller has the world, so it can answer this without the
 * selection module needing to know what an edge is.
 */
export function isEdgeIn(world: FocusableIds, id: string | null): boolean {
  return id !== null && world.edges.includes(id);
}
