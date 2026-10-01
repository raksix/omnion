/**
 * Who is selected on the canvas (REQ-004 slice 2).
 *
 * Selection is one piece of state with four writers — a click, a Shift+click, a marquee and
 * `⌘A` — plus a fifth kind of thing that can be selected at all: an *edge*. Until this
 * module existed those five answers lived in three `useState` calls and one ref, and every
 * writer assembled "what is selected" from whatever combination of them it happened to
 * remember. Two bugs came out of that, and both were found by the browser rather than by a
 * test, because none of the logic was in a place a test could reach:
 *
 * * **Shift+click could not deselect.** Toggling a node *out* of the multi-selection still
 *   re-pointed the focus at it, so the card stayed drawn as selected and no second press
 *   could clear it. The gesture and the highlight disagreed about what had just happened.
 * * **`Del` could not be reasoned about.** An edge wins over a node selection, because a user
 *   pointing at a line means the line — but "wins" was a branch in a hundred-line key handler
 *   rather than a rule, so nothing asserted it and nothing could break it visibly.
 *
 * The rule this module encodes: **one answer, five ways to reach it.** Every writer returns a
 * new state through a named transition, and the two questions the rest of the builder asks —
 * "is this node drawn as selected?" and "what does `Del` remove?" — are answered by functions
 * that cannot be forgotten by a caller.
 */

/** What the canvas considers selected right now. */
export interface CanvasSelection {
  /**
   * The multi-selection: the nodes a marquee caught or a Shift+click accumulated. Kept
   * separate from `focus` because the two answer different questions — the outline is drawn
   * from this, the inspector from that.
   */
  readonly nodes: readonly string[];
  /**
   * The one node the inspector shows and the arrow keys nudge. Not necessarily in `nodes`:
   * a plain click focuses a single card without turning a group into a selection.
   */
  readonly focus: string | null;
  /**
   * The edge selected for deletion, held apart on purpose. An edge is selected to be
   * *removed*, and letting it drive the inspector would replace a half-configured node with a
   * panel that has nothing to say about a line.
   */
  readonly edge: string | null;
}

/** Nothing is selected. Shared as a constant so a re-render does not allocate a new object. */
export const EMPTY_SELECTION: CanvasSelection = { nodes: [], focus: null, edge: null };

/** A fresh selection from a plain click: one focused node, no group, no edge. */
export function selectNode(id: string): CanvasSelection {
  return { nodes: [], focus: id, edge: null };
}

/** A fresh selection from a marquee: the group, with its last node also driving the inspector. */
export function selectGroup(ids: readonly string[]): CanvasSelection {
  const unique = uniqueIds(ids);
  if (unique.length === 0) {
    return { ...EMPTY_SELECTION };
  }
  return { nodes: unique, focus: unique[unique.length - 1], edge: null };
}

/** A fresh selection from `⌘A`: every node, focused on the last so the inspector has content. */
export function selectAll(ids: readonly string[]): CanvasSelection {
  return selectGroup(ids);
}

/**
 * Shift+click: add the node to the group, or take it out again.
 *
 * Toggling *out* also drops the focus when the focused node was the one leaving — otherwise
 * the card the user just de-selected would still be drawn with the single-selection outline
 * and the second Shift+click would appear to do nothing. This is the whole reason the
 * transitions live here: the two answers (group membership and focus) have to change
 * *together*, and in a component that is three lines apart in three different writers.
 */
export function toggleNode(current: CanvasSelection, id: string): CanvasSelection {
  if (current.nodes.includes(id)) {
    const nodes = current.nodes.filter((candidate) => candidate !== id);
    return {
      nodes,
      focus: current.focus === id ? (nodes[nodes.length - 1] ?? null) : current.focus,
      edge: null,
    };
  }
  // Adding does *not* steal the focus from whatever a plain click had already focused. That
  // is the whole meaning of the modifier: click A, Shift+click B, and both are selected. The
  // focus stays where the user put it — the inspector keeps showing the node they were last
  // reading — and only a focus that is somehow empty falls back to the new node.
  return { nodes: [...current.nodes, id], focus: current.focus ?? id, edge: null };
}

/** Shift-marquee: the band adds to whatever is already selected rather than replacing it. */
export function extendGroup(current: CanvasSelection, ids: readonly string[]): CanvasSelection {
  // Extended from the *drawn* selection, not from `nodes`: after a plain click the group is
  // empty but the clicked card is still outlined, and replacing it would make Shift+click
  // the exact opposite of what it is for.
  return selectGroup([...membersOf(current), ...ids]);
}

/** An edge was clicked: it becomes the delete target and the node selection steps aside. */
export function selectEdge(current: CanvasSelection, id: string): CanvasSelection {
  return { nodes: [], focus: null, edge: id };
}

/** Clicking the desk: nothing is selected. */
export function clearSelection(): CanvasSelection {
  return { ...EMPTY_SELECTION };
}

/**
 * Escape: cancel a connection in progress, else an edge, else the nodes.
 *
 * Returned as a *step* rather than applied, because the caller has to ask about the
 * connection draft first — Escape is the gesture that says "I did not mean that", and it has
 * to reach the thing the user is actually holding rather than the thing that was selected
 * three gestures ago.
 */
export type EscapeStep = "connection" | "edge" | "nodes" | "nothing";

export function whatEscapeClears(
  current: CanvasSelection,
  connectionInFlight: boolean,
): EscapeStep {
  if (connectionInFlight) {
    return "connection";
  }
  if (current.edge) {
    return "edge";
  }
  if (current.nodes.length > 0 || current.focus) {
    return "nodes";
  }
  return "nothing";
}

/**
 * Should the key handler consume this Escape?
 *
 * No. A handler that `preventDefault()`s an Escape with nothing selected swallows the
 * browser's own dismiss — the palette's search box, a dialog the user opened with the same
 * key — and the user is left pressing Escape again at a page that looks stuck.
 */
export function escapeStepIsHandled(step: EscapeStep): boolean {
  return step !== "nothing";
}

/**
 * What `Del` (or Backspace) removes.
 *
 * An edge wins over a node selection, because the user is pointing at a line and means the
 * line; a group with no focus removes every member at once, so one press of undo comes back
 * the whole group rather than one node per press.
 */
export type DeleteTarget =
  | { kind: "edge"; id: string }
  | { kind: "nodes"; ids: string[] }
  | { kind: "nothing" };

export function deleteTarget(current: CanvasSelection): DeleteTarget {
  if (current.edge) {
    return { kind: "edge", id: current.edge };
  }
  // `membersOf`, not a second copy of the same rule: "what Del removes" and "what is drawn
  // as selected" have to be the same set, or the user deletes a node they cannot see selected.
  const ids = membersOf(current);
  return ids.length > 0 ? { kind: "nodes", ids } : { kind: "nothing" };
}

/**
 * Is this card drawn as selected?
 *
 * One function, so the canvas outline, the minimap dot and the keyboard walk cannot disagree
 * about a third definition of "selected" — a disagreement that is invisible in a screenshot
 * and impossible to see in a test written against a CSS string.
 */
export function isNodeSelected(current: CanvasSelection, id: string): boolean {
  return current.focus === id || current.nodes.includes(id);
}

/** How many nodes are drawn as selected — what the status bar and the minimap report. */
export function selectionSize(current: CanvasSelection): number {
  return membersOf(current).length;
}

/**
 * The nodes `Del` would remove — the *drawn* selection, focus included.
 *
 * The union, not `nodes` alone. After a plain click on A and a Shift+click on B, two cards
 * are outlined (the group holds B, the focus holds A) and `Del` has to take both: a delete
 * that removed one of two highlighted cards is worse than no delete, because the user cannot
 * see which one it meant. This is the single definition of "the selection" that the outline,
 * the minimap, the status bar and the delete key all read.
 */
export function membersOf(current: CanvasSelection): string[] {
  return uniqueIds([...current.nodes, ...(current.focus ? [current.focus] : [])]);
}

/**
 * Drop every id that no longer exists, so a delete cannot leave a phantom focus behind.
 *
 * ## Why the edge is a third argument and not a detail
 *
 * The node half of this rule is the one the acceptance criteria talk about, and it was the one
 * that got fixed: a selection naming a card the graph no longer has leaves the inspector blank,
 * keeps Duplicate and Copy enabled (`disabled={!selected}` reads the surviving string) and
 * makes `Del` resolve to nothing. This function is where that was fixed.
 *
 * The edge sat outside it for two ticks, and it is not a corner: an edge is one of the two
 * things that can be selected at all, and it **outranks every node selection** in
 * `deleteTarget` and in `whatEscapeClears`. So a surviving `edge` id is not cosmetic — it is
 * what `Del` resolves to, and the status bar renders "1 connection selected (Del removes it)"
 * from it.
 *
 * `aliveEdges` is **optional**, and that is the design rather than an oversight. A caller that
 * only knows the node set — `removeNodes` prunes against `nextNodes.map(n => n.id)` and has no
 * edge list in hand — cannot answer the question, and a prune that answered "no" for every
 * unknown would throw away a selection the caller had no reason to doubt. So: handed the set,
 * decide; handed nothing, do not guess. `rebaseAfterReload` is the caller that has it, and it
 * is the one that must, because the graph it just adopted is **the other editor's** and their
 * removed connections are the ordinary case.
 */
export function pruneSelection(
  current: CanvasSelection,
  alive: readonly string[],
  aliveEdges?: readonly string[],
): CanvasSelection {
  const present = new Set(alive);
  const nodes = current.nodes.filter((id) => present.has(id));
  const focus = current.focus && present.has(current.focus) ? current.focus : (nodes[0] ?? null);
  const edge =
    current.edge !== null && aliveEdges !== undefined && !aliveEdges.includes(current.edge)
      ? null
      : current.edge;
  return { nodes, focus, edge };
}

/**
 * The roving focus order: every node, then every edge.
 *
 * A keyboard pass has to be able to *reach* an edge, or "Del on a selected edge removes it"
 * is a criterion only a pointer can satisfy. Nodes come first because that is the order the
 * graph reads in, and the edges follow so the walk ends where the drawing does.
 */
export interface FocusableIds {
  readonly nodes: readonly string[];
  readonly edges: readonly string[];
}

export function focusOrder(world: FocusableIds): string[] {
  return [...world.nodes, ...world.edges];
}

function uniqueIds(ids: readonly string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const id of ids) {
    if (!seen.has(id)) {
      seen.add(id);
      out.push(id);
    }
  }
  return out;
}
